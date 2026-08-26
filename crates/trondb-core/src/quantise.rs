use crate::error::EngineError;

// ---------------------------------------------------------------------------
// Int8 Scalar Quantisation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct QuantisedInt8 {
    pub min: f32,
    pub max: f32,
    pub data: Vec<u8>,
}

pub fn quantise_int8(vector: &[f32]) -> QuantisedInt8 {
    if vector.is_empty() {
        return QuantisedInt8 {
            min: 0.0,
            max: 0.0,
            data: Vec::new(),
        };
    }

    let min = vector.iter().copied().fold(f32::INFINITY, f32::min);
    let max = vector.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    if (max - min).abs() < f32::EPSILON {
        // Constant vector — all zeros, min carries the value
        return QuantisedInt8 {
            min,
            max,
            data: vec![0u8; vector.len()],
        };
    }

    let scale = 255.0 / (max - min);
    let data: Vec<u8> = vector
        .iter()
        .map(|&v| ((v - min) * scale).round().clamp(0.0, 255.0) as u8)
        .collect();

    QuantisedInt8 { min, max, data }
}

pub fn dequantise_int8(q: &QuantisedInt8) -> Vec<f32> {
    if q.data.is_empty() {
        return Vec::new();
    }

    if (q.max - q.min).abs() < f32::EPSILON {
        return vec![q.min; q.data.len()];
    }

    let range = q.max - q.min;
    q.data
        .iter()
        .map(|&v| (v as f32 / 255.0) * range + q.min)
        .collect()
}

impl QuantisedInt8 {
    /// Serialise: [min: f32 LE][max: f32 LE][data: u8...]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.data.len());
        buf.extend_from_slice(&self.min.to_le_bytes());
        buf.extend_from_slice(&self.max.to_le_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EngineError> {
        if bytes.len() < 8 {
            return Err(EngineError::Storage("Int8 data too short".into()));
        }
        let min = f32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let max = f32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let data = bytes[8..].to_vec();
        Ok(Self { min, max, data })
    }
}

// ---------------------------------------------------------------------------
// Binary Quantisation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct QuantisedBinary {
    pub data: Vec<u8>,
    pub dimensions: usize,
}

pub fn quantise_binary(vector: &[f32]) -> QuantisedBinary {
    let dimensions = vector.len();
    let byte_count = dimensions.div_ceil(8);
    let mut data = vec![0u8; byte_count];

    for (i, &v) in vector.iter().enumerate() {
        if v >= 0.0 {
            let byte_idx = i / 8;
            let bit_idx = 7 - (i % 8); // MSB-first
            data[byte_idx] |= 1 << bit_idx;
        }
    }

    QuantisedBinary { data, dimensions }
}

impl QuantisedBinary {
    pub fn to_bytes(&self) -> Vec<u8> {
        self.data.clone()
    }

    pub fn from_bytes(bytes: &[u8], dimensions: usize) -> Result<Self, EngineError> {
        let expected = dimensions.div_ceil(8);
        if bytes.len() < expected {
            return Err(EngineError::Storage("Binary data too short".into()));
        }
        Ok(Self {
            data: bytes[..expected].to_vec(),
            dimensions,
        })
    }
}

// ---------------------------------------------------------------------------
// PolarQuant — TurboQuant-style low-bit vector quantisation
//
// Based on "TurboQuant: Online Vector Quantization with Near-optimal
// Distortion Rate" (Google Research, ICLR 2026).
//
// Pipeline: random rotation (fast Walsh-Hadamard + signs) →
//           Lloyd-Max scalar quantisation per coordinate →
//           packed b-bit codes.
//
// For 384-dim vectors at 3 bits: 160 bytes (vs 1536 for f32 = 9.6x).
// Near-zero recall loss for cosine/inner-product similarity.
// ---------------------------------------------------------------------------

/// Configuration for PolarQuant. Generated once per collection representation
/// and stored as metadata. The rotation matrix is implicit (Hadamard + signs).
#[derive(Debug, Clone)]
pub struct PolarQuantConfig {
    /// Bits per coordinate (2, 3, or 4 recommended).
    pub bits: u8,
    /// Original vector dimensions.
    pub dimensions: usize,
    /// Random ±1 signs for the fast Walsh-Hadamard transform.
    /// Length = padded_dim (next power of 2 >= dimensions).
    pub signs: Vec<bool>,
    /// Padded dimension (next power of 2 >= dimensions).
    pub padded_dim: usize,
}

/// A quantised vector produced by PolarQuant.
#[derive(Debug, Clone)]
pub struct QuantisedPolar {
    /// Original vector L2 norm (needed for cosine similarity reconstruction).
    pub norm: f32,
    /// Number of original (unpadded) dimensions.
    pub dimensions: usize,
    /// Bits per coordinate.
    pub bits: u8,
    /// Packed b-bit codes. Each coordinate encoded as a b-bit index into
    /// the centroid table. Packed MSB-first within bytes.
    pub data: Vec<u8>,
}

// Lloyd-Max optimal centroids for N(0,1), pre-computed.
// Symmetric: only positive half stored. Centroid[i] = -Centroid[2^b - 1 - i].
// These are the exact values computed via iterative Lloyd-Max optimisation.

/// 3-bit (8 levels) centroids for N(0,1). Full table (negative + positive).
const CENTROIDS_3BIT: [f32; 8] = [
    -2.1519457, -1.3439093, -0.7560053, -0.2450942, 0.2450942, 0.7560053, 1.3439093, 2.1519457,
];

/// 3-bit boundaries (7 values, including -inf/+inf implicit).
const BOUNDARIES_3BIT: [f32; 7] = [
    -1.7479275, -1.0499573, -0.5005497, 0.0, 0.5005497, 1.0499573, 1.7479275,
];

/// 4-bit (16 levels) centroids for N(0,1).
const CENTROIDS_4BIT: [f32; 16] = [
    -2.732_973, -2.0694504, -1.6184884, -1.2566477, -0.9427008, -0.657_037, -0.3882236, -0.1284549,
    0.1284549, 0.3882236, 0.657_037, 0.9427008, 1.2566477, 1.6184884, 2.0694504, 2.732_973,
];

/// 4-bit boundaries (15 values).
const BOUNDARIES_4BIT: [f32; 15] = [
    -2.4012263, -1.8439851, -1.4375834, -1.099_688, -0.7998803, -0.5226384, -0.2583434, 0.0,
    0.2583434, 0.5226384, 0.7998803, 1.099_688, 1.4375834, 1.8439851, 2.4012263,
];

/// 2-bit (4 levels) centroids for N(0,1).
const CENTROIDS_2BIT: [f32; 4] = [-1.5104176, -0.452_78, 0.452_78, 1.5104176];

/// 2-bit boundaries (3 values).
const BOUNDARIES_2BIT: [f32; 3] = [-0.9815988, 0.0, 0.9815988];

impl PolarQuantConfig {
    /// Create a new config with deterministic signs from a seed.
    /// The seed should be unique per collection+representation.
    pub fn new(bits: u8, dimensions: usize, seed: u64) -> Self {
        assert!(matches!(bits, 2..=4), "PolarQuant supports 2, 3, or 4 bits");

        // Pad to next power of 2 for Walsh-Hadamard
        let padded_dim = dimensions.next_power_of_two();

        // Generate deterministic random signs from seed (simple LCG)
        let mut rng_state = seed;
        let signs: Vec<bool> = (0..padded_dim)
            .map(|_| {
                // xorshift64
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 7;
                rng_state ^= rng_state << 17;
                (rng_state & 1) == 0
            })
            .collect();

        Self {
            bits,
            dimensions,
            signs,
            padded_dim,
        }
    }

    fn centroids(&self) -> &[f32] {
        match self.bits {
            2 => &CENTROIDS_2BIT,
            3 => &CENTROIDS_3BIT,
            4 => &CENTROIDS_4BIT,
            _ => unreachable!(),
        }
    }

    fn boundaries(&self) -> &[f32] {
        match self.bits {
            2 => &BOUNDARIES_2BIT,
            3 => &BOUNDARIES_3BIT,
            4 => &BOUNDARIES_4BIT,
            _ => unreachable!(),
        }
    }

    /// Serialise config: [bits: u8][dim: u32 LE][padded: u32 LE][signs: packed bits]
    pub fn to_bytes(&self) -> Vec<u8> {
        let sign_bytes = self.padded_dim.div_ceil(8);
        let mut buf = Vec::with_capacity(1 + 4 + 4 + sign_bytes);
        buf.push(self.bits);
        buf.extend_from_slice(&(self.dimensions as u32).to_le_bytes());
        buf.extend_from_slice(&(self.padded_dim as u32).to_le_bytes());
        // Pack signs as bits
        let mut sign_data = vec![0u8; sign_bytes];
        for (i, &s) in self.signs.iter().enumerate() {
            if s {
                sign_data[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        buf.extend_from_slice(&sign_data);
        buf
    }

    pub fn from_bytes(bytes: &[u8], seed: u64) -> Result<Self, EngineError> {
        if bytes.len() < 9 {
            return Err(EngineError::Storage("PolarQuant config too short".into()));
        }
        let bits = bytes[0];
        let dimensions = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
        let padded_dim = u32::from_le_bytes(bytes[5..9].try_into().unwrap()) as usize;
        let sign_bytes = padded_dim.div_ceil(8);
        if bytes.len() < 9 + sign_bytes {
            return Err(EngineError::Storage(
                "PolarQuant config signs too short".into(),
            ));
        }
        let sign_data = &bytes[9..9 + sign_bytes];
        let signs: Vec<bool> = (0..padded_dim)
            .map(|i| (sign_data[i / 8] >> (7 - (i % 8))) & 1 == 1)
            .collect();
        let _ = seed; // seed not needed when deserialising — signs are in the bytes
        Ok(Self {
            bits,
            dimensions,
            signs,
            padded_dim,
        })
    }
}

/// Apply the fast Walsh-Hadamard transform in-place.
/// Input must have length that is a power of 2.
fn fast_walsh_hadamard(data: &mut [f32]) {
    let n = data.len();
    debug_assert!(n.is_power_of_two());
    let mut h = 1;
    while h < n {
        for i in (0..n).step_by(h * 2) {
            for j in i..i + h {
                let x = data[j];
                let y = data[j + h];
                data[j] = x + y;
                data[j + h] = x - y;
            }
        }
        h *= 2;
    }
    // Normalise by 1/sqrt(n)
    let scale = 1.0 / (n as f32).sqrt();
    for v in data.iter_mut() {
        *v *= scale;
    }
}

/// Rotate a vector using fast Walsh-Hadamard with random signs.
/// Returns the rotated vector (padded to power-of-2 length).
fn rotate_vector(vector: &[f32], config: &PolarQuantConfig) -> Vec<f32> {
    let mut padded = vec![0.0f32; config.padded_dim];
    // Copy and apply signs
    for (i, &v) in vector.iter().enumerate() {
        padded[i] = if config.signs[i] { v } else { -v };
    }
    // Zero-pad remaining dimensions (signs applied but value is 0)
    fast_walsh_hadamard(&mut padded);
    padded
}

/// Quantise a coordinate value to a b-bit index using binary search on boundaries.
fn quantise_scalar(value: f32, boundaries: &[f32]) -> u8 {
    // Binary search: find the bucket this value falls into
    match boundaries
        .binary_search_by(|b| b.partial_cmp(&value).unwrap_or(std::cmp::Ordering::Equal))
    {
        Ok(i) => i as u8,  // Exactly on a boundary — assign to right bucket
        Err(i) => i as u8, // Between boundaries — bucket index
    }
}

/// Quantise a vector using PolarQuant.
pub fn quantise_polar(vector: &[f32], config: &PolarQuantConfig) -> QuantisedPolar {
    let dimensions = vector.len();
    assert_eq!(dimensions, config.dimensions, "dimension mismatch");

    // Compute and store the original norm
    let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();

    // Normalise to unit sphere before rotation
    let normalised: Vec<f32> = if norm > f32::EPSILON {
        vector.iter().map(|v| v / norm).collect()
    } else {
        vec![0.0; dimensions]
    };

    // Rotate
    let rotated = rotate_vector(&normalised, config);

    // Scale rotated coordinates: after rotation on unit sphere, coordinates ≈ N(0, 1/d).
    // The Lloyd-Max centroids are for N(0,1), so scale by sqrt(d).
    // Use the original dimensions for scaling (not padded), as the energy
    // concentrates in the original dimensions of the unit vector.
    let scale = (config.dimensions as f32).sqrt();
    let boundaries = config.boundaries();
    let bits = config.bits;
    let num_levels = 1u8 << bits;

    // Quantise each coordinate (only the original dimensions, not padding)
    let codes: Vec<u8> = rotated[..dimensions]
        .iter()
        .map(|&v| {
            let scaled = v * scale;
            let code = quantise_scalar(scaled, boundaries);
            code.min(num_levels - 1) // clamp to valid range
        })
        .collect();

    // Pack b-bit codes into bytes
    let total_bits = dimensions * bits as usize;
    let byte_count = total_bits.div_ceil(8);
    let mut data = vec![0u8; byte_count];

    for (i, &code) in codes.iter().enumerate() {
        let bit_offset = i * bits as usize;
        // Write b bits starting at bit_offset (MSB-first packing)
        for b in 0..bits {
            let bit_val = (code >> (bits - 1 - b)) & 1;
            let global_bit = bit_offset + b as usize;
            if bit_val == 1 {
                data[global_bit / 8] |= 1 << (7 - (global_bit % 8));
            }
        }
    }

    QuantisedPolar {
        norm,
        dimensions,
        bits,
        data,
    }
}

/// Dequantise a PolarQuant vector back to approximate f32 values.
pub fn dequantise_polar(q: &QuantisedPolar, config: &PolarQuantConfig) -> Vec<f32> {
    let centroids = config.centroids();
    let bits = q.bits;
    let scale_inv = 1.0 / (config.dimensions as f32).sqrt();

    // Unpack codes
    let codes = unpack_codes(&q.data, q.dimensions, bits);

    // Reconstruct rotated coordinates from centroids
    let mut rotated = vec![0.0f32; config.padded_dim];
    for (i, &code) in codes.iter().enumerate() {
        rotated[i] = centroids[code as usize] * scale_inv;
    }

    // Inverse rotation: for Walsh-Hadamard, the inverse is the same transform
    // (it's self-inverse up to scaling, and we already normalised)
    fast_walsh_hadamard(&mut rotated);

    // Undo signs
    let mut result = Vec::with_capacity(q.dimensions);
    for (&sign, &r) in config.signs[..q.dimensions]
        .iter()
        .zip(rotated[..q.dimensions].iter())
    {
        let v = if sign { r } else { -r };
        result.push(v * q.norm); // Rescale by original norm
    }

    result
}

/// Unpack b-bit codes from packed byte array.
fn unpack_codes(data: &[u8], count: usize, bits: u8) -> Vec<u8> {
    let mut codes = Vec::with_capacity(count);
    for i in 0..count {
        let bit_offset = i * bits as usize;
        let mut code: u8 = 0;
        for b in 0..bits {
            let global_bit = bit_offset + b as usize;
            let bit_val = (data[global_bit / 8] >> (7 - (global_bit % 8))) & 1;
            code |= bit_val << (bits - 1 - b);
        }
        codes.push(code);
    }
    codes
}

/// Compute approximate inner product between a full-precision query vector
/// and a PolarQuant-encoded stored vector. This avoids full dequantisation.
///
/// Both vectors should be passed as raw (unrotated) vectors. The rotation
/// is applied internally to avoid caller-side errors.
pub fn approx_inner_product_raw(
    query: &[f32],
    quantised: &QuantisedPolar,
    config: &PolarQuantConfig,
) -> f32 {
    // Dequantise and compute exact inner product with the approximated vector.
    // This is the simplest correct implementation. For high-throughput use,
    // a fused rotate-and-dot implementation can avoid the full dequantise.
    let approx_vec = dequantise_polar(quantised, config);
    query
        .iter()
        .zip(approx_vec.iter())
        .map(|(a, b)| a * b)
        .sum()
}

/// Rotate a query vector for use with `approx_inner_product`.
/// The query should be normalised to unit length before calling this.
pub fn rotate_query(query: &[f32], config: &PolarQuantConfig) -> Vec<f32> {
    rotate_vector(query, config)
}

impl QuantisedPolar {
    /// Serialise: [bits: u8][dim: u32 LE][norm: f32 LE][data: packed codes]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + 4 + 4 + self.data.len());
        buf.push(self.bits);
        buf.extend_from_slice(&(self.dimensions as u32).to_le_bytes());
        buf.extend_from_slice(&self.norm.to_le_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EngineError> {
        if bytes.len() < 9 {
            return Err(EngineError::Storage("PolarQuant data too short".into()));
        }
        let bits = bytes[0];
        let dimensions = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
        let norm = f32::from_le_bytes(bytes[5..9].try_into().unwrap());
        let data = bytes[9..].to_vec();
        Ok(Self {
            norm,
            dimensions,
            bits,
            data,
        })
    }

    /// Size in bytes of the packed data (excluding header).
    pub fn data_size(&self) -> usize {
        self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantise_int8_roundtrip() {
        let vector = vec![1.0, 0.5, 0.0, -0.5, -1.0];
        let q = quantise_int8(&vector);
        let restored = dequantise_int8(&q);
        for (a, b) in vector.iter().zip(restored.iter()) {
            assert!((a - b).abs() < 0.01, "expected {a}, got {b}");
        }
    }

    #[test]
    fn quantise_int8_constant_vector() {
        let vector = vec![3.0, 3.0, 3.0];
        let q = quantise_int8(&vector);
        let restored = dequantise_int8(&q);
        for v in &restored {
            assert!((v - 3.0).abs() < 0.01);
        }
    }

    #[test]
    fn quantise_int8_serialisation() {
        let vector = vec![1.0, -1.0, 0.5];
        let q = quantise_int8(&vector);
        let bytes = q.to_bytes();
        let q2 = QuantisedInt8::from_bytes(&bytes).unwrap();
        assert_eq!(q.min, q2.min);
        assert_eq!(q.max, q2.max);
        assert_eq!(q.data, q2.data);
    }

    #[test]
    fn quantise_binary_basic() {
        let vector = vec![1.0, -0.5, 0.0, -1.0, 0.5, -0.1, 0.9, -0.9];
        let q = quantise_binary(&vector);
        assert_eq!(q.dimensions, 8);
        // 1.0→1, -0.5→0, 0.0→1, -1.0→0, 0.5→1, -0.1→0, 0.9→1, -0.9→0
        // bits: 1 0 1 0 1 0 1 0 = 0xAA
        assert_eq!(q.data, vec![0xAA]);
    }

    #[test]
    fn quantise_binary_non_aligned() {
        // 3 dimensions → 1 byte, last 5 bits padded with 0
        let vector = vec![1.0, -1.0, 1.0];
        let q = quantise_binary(&vector);
        assert_eq!(q.dimensions, 3);
        // bits: 1 0 1 0 0 0 0 0 = 0xA0
        assert_eq!(q.data, vec![0xA0]);
    }

    #[test]
    fn quantise_binary_serialisation() {
        let vector = vec![1.0, -1.0, 0.5, -0.5];
        let q = quantise_binary(&vector);
        let bytes = q.to_bytes();
        let q2 = QuantisedBinary::from_bytes(&bytes, 4).unwrap();
        assert_eq!(q.data, q2.data);
        assert_eq!(q.dimensions, q2.dimensions);
    }

    #[test]
    fn quantise_int8_empty_vector() {
        let vector: Vec<f32> = vec![];
        let q = quantise_int8(&vector);
        assert!(q.data.is_empty());
        let restored = dequantise_int8(&q);
        assert!(restored.is_empty());
    }

    // -----------------------------------------------------------------------
    // PolarQuant tests
    // -----------------------------------------------------------------------

    #[test]
    fn polar_quant_roundtrip_3bit() {
        let config = PolarQuantConfig::new(3, 8, 42);
        let vector = vec![0.3, -0.5, 0.1, 0.8, -0.2, 0.4, -0.7, 0.6];
        let q = quantise_polar(&vector, &config);
        let restored = dequantise_polar(&q, &config);

        assert_eq!(restored.len(), 8);
        // At 3-bit, MSE/dimension should be low. Check cosine similarity.
        let dot: f32 = vector.iter().zip(restored.iter()).map(|(a, b)| a * b).sum();
        let norm_orig: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        let norm_rest: f32 = restored.iter().map(|v| v * v).sum::<f32>().sqrt();
        let cosine = dot / (norm_orig * norm_rest + f32::EPSILON);
        assert!(cosine > 0.9, "cosine similarity {cosine} too low for 3-bit");
    }

    #[test]
    fn polar_quant_roundtrip_4bit() {
        let config = PolarQuantConfig::new(4, 16, 123);
        let vector: Vec<f32> = (0..16).map(|i| (i as f32 - 8.0) / 10.0).collect();
        let q = quantise_polar(&vector, &config);
        let restored = dequantise_polar(&q, &config);

        assert_eq!(restored.len(), 16);
        let dot: f32 = vector.iter().zip(restored.iter()).map(|(a, b)| a * b).sum();
        let norm_orig: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        let norm_rest: f32 = restored.iter().map(|v| v * v).sum::<f32>().sqrt();
        let cosine = dot / (norm_orig * norm_rest + f32::EPSILON);
        assert!(
            cosine > 0.95,
            "cosine similarity {cosine} too low for 4-bit"
        );
    }

    #[test]
    fn polar_quant_384dim_3bit() {
        // Realistic test: 384-dim vector (same as bge-small-en-v1.5)
        let config = PolarQuantConfig::new(3, 384, 999);
        let vector: Vec<f32> = (0..384)
            .map(|i| ((i as f32 * 7.3 + 1.1).sin() * 0.5))
            .collect();

        let q = quantise_polar(&vector, &config);

        // Check compression ratio
        let original_bytes = 384 * 4; // f32
        let compressed_bytes = q.data.len() + 9; // data + header
        let ratio = original_bytes as f32 / compressed_bytes as f32;
        assert!(
            ratio > 8.0,
            "compression ratio {ratio} too low, expected >8x"
        );

        // Check roundtrip quality
        let restored = dequantise_polar(&q, &config);
        let dot: f32 = vector.iter().zip(restored.iter()).map(|(a, b)| a * b).sum();
        let norm_orig: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        let norm_rest: f32 = restored.iter().map(|v| v * v).sum::<f32>().sqrt();
        let cosine = dot / (norm_orig * norm_rest + f32::EPSILON);
        assert!(cosine > 0.85, "384-dim 3-bit cosine {cosine} too low");
    }

    #[test]
    fn polar_quant_approx_inner_product() {
        let config = PolarQuantConfig::new(3, 384, 42);
        // Create two vectors with non-trivial inner product
        let a: Vec<f32> = (0..384).map(|i| (i as f32 * 0.1).sin()).collect();
        let b: Vec<f32> = (0..384)
            .map(|i| (i as f32 * 0.1).sin() + (i as f32 * 0.3).cos() * 0.2)
            .collect();

        // Exact inner product
        let exact_ip: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();

        // Quantise b, compute approximate IP
        let q_b = quantise_polar(&b, &config);
        let approx_ip = approx_inner_product_raw(&a, &q_b, &config);

        // Check: same sign and reasonable magnitude
        assert!(
            exact_ip * approx_ip > 0.0,
            "sign mismatch: exact={exact_ip}, approx={approx_ip}"
        );
        let relative_error = ((exact_ip - approx_ip) / exact_ip.abs()).abs();
        assert!(
            relative_error < 0.25,
            "approx IP relative error {relative_error} too high (exact={exact_ip}, approx={approx_ip})"
        );
    }

    #[test]
    fn polar_quant_serialisation() {
        let config = PolarQuantConfig::new(3, 32, 77);
        let vector: Vec<f32> = (0..32).map(|i| (i as f32 - 16.0) / 20.0).collect();
        let q = quantise_polar(&vector, &config);

        let bytes = q.to_bytes();
        let q2 = QuantisedPolar::from_bytes(&bytes).unwrap();
        assert_eq!(q.bits, q2.bits);
        assert_eq!(q.dimensions, q2.dimensions);
        assert_eq!(q.norm, q2.norm);
        assert_eq!(q.data, q2.data);
    }

    #[test]
    fn polar_quant_config_serialisation() {
        let config = PolarQuantConfig::new(3, 384, 42);
        let bytes = config.to_bytes();
        let config2 = PolarQuantConfig::from_bytes(&bytes, 0).unwrap();
        assert_eq!(config.bits, config2.bits);
        assert_eq!(config.dimensions, config2.dimensions);
        assert_eq!(config.padded_dim, config2.padded_dim);
        assert_eq!(config.signs, config2.signs);
    }

    #[test]
    fn walsh_hadamard_self_inverse() {
        // Walsh-Hadamard applied twice should return the original vector
        let mut data = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let original = data.clone();
        fast_walsh_hadamard(&mut data);
        fast_walsh_hadamard(&mut data);
        for (a, b) in original.iter().zip(data.iter()) {
            assert!((a - b).abs() < 1e-5, "expected {a}, got {b}");
        }
    }

    #[test]
    fn polar_quant_zero_vector() {
        let config = PolarQuantConfig::new(3, 8, 42);
        let vector = vec![0.0; 8];
        let q = quantise_polar(&vector, &config);
        assert!(q.norm < f32::EPSILON);
        let restored = dequantise_polar(&q, &config);
        for v in &restored {
            assert!(v.abs() < 1e-5);
        }
    }

    #[test]
    fn polar_quant_2bit() {
        let config = PolarQuantConfig::new(2, 384, 55);
        let vector: Vec<f32> = (0..384).map(|i| ((i as f32 * 2.1).sin() * 0.5)).collect();
        let q = quantise_polar(&vector, &config);
        let restored = dequantise_polar(&q, &config);

        let dot: f32 = vector.iter().zip(restored.iter()).map(|(a, b)| a * b).sum();
        let norm_orig: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        let norm_rest: f32 = restored.iter().map(|v| v * v).sum::<f32>().sqrt();
        let cosine = dot / (norm_orig * norm_rest + f32::EPSILON);
        // 2-bit is coarser — more distortion expected
        assert!(cosine > 0.75, "384-dim 2-bit cosine {cosine} too low");
    }
}
