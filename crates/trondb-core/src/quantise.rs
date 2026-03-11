use crate::error::EngineError;

// ---------------------------------------------------------------------------
// Int8 Scalar Quantisation
// ---------------------------------------------------------------------------

pub struct QuantisedInt8 {
    pub min: f32,
    pub max: f32,
    pub data: Vec<u8>,
}

pub fn quantise_int8(vector: &[f32]) -> QuantisedInt8 {
    if vector.is_empty() {
        return QuantisedInt8 { min: 0.0, max: 0.0, data: Vec::new() };
    }

    let min = vector.iter().copied().fold(f32::INFINITY, f32::min);
    let max = vector.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    if (max - min).abs() < f32::EPSILON {
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

pub struct QuantisedBinary {
    pub data: Vec<u8>,
    pub dimensions: usize,
}

pub fn quantise_binary(vector: &[f32]) -> QuantisedBinary {
    let dimensions = vector.len();
    let byte_count = (dimensions + 7) / 8;
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
        let expected = (dimensions + 7) / 8;
        if bytes.len() < expected {
            return Err(EngineError::Storage("Binary data too short".into()));
        }
        Ok(Self {
            data: bytes[..expected].to_vec(),
            dimensions,
        })
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
}
