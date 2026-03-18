#![allow(unused_imports)]
use async_trait::async_trait;
use std::path::Path;

use trondb_core::types::VectorData;
use trondb_core::vectoriser::{FieldSet, VectorKind, Vectoriser, VectoriserError};

#[cfg(feature = "onnx")]
use ort::execution_providers::CUDAExecutionProvider;
#[cfg(feature = "onnx")]
use ort::session::builder::GraphOptimizationLevel;
#[cfg(feature = "onnx")]
use ort::session::Session;
#[cfg(feature = "onnx")]
use ort::value::TensorRef;
#[cfg(feature = "onnx")]
use tokenizers::Tokenizer;

#[cfg(feature = "onnx")]
pub struct OnnxDenseVectoriser {
    id: String,
    model_id: String,
    session: std::sync::Mutex<Session>,
    tokenizer: Tokenizer,
    dimensions: usize,
}

#[cfg(feature = "onnx")]
impl OnnxDenseVectoriser {
    pub fn new(
        model_id: &str,
        model_path: &Path,
        tokenizer_path: &Path,
        dimensions: usize,
    ) -> Result<Self, VectoriserError> {
        // Try CUDA first, fall back to CPU if GPU isn't available
        let builder = Session::builder()
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?;
        let session = match builder
            .with_execution_providers([CUDAExecutionProvider::default().build()])
        {
            Ok(b) => match b
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|e| e.to_string())
                .and_then(|mut b| b.commit_from_file(model_path).map_err(|e| e.to_string()))
            {
                Ok(s) => {
                    tracing::info!(model = model_id, "ONNX dense vectoriser loaded");
                    s
                }
                Err(e) => {
                    tracing::warn!(model = model_id, error = %e, "CUDA session failed, falling back to CPU");
                    Session::builder()
                        .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?
                        .with_optimization_level(GraphOptimizationLevel::Level3)
                        .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?
                        .commit_from_file(model_path)
                        .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?
                }
            }
            Err(e) => {
                tracing::warn!(model = model_id, error = %e, "CUDA unavailable, falling back to CPU");
                Session::builder()
                    .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?
                    .with_optimization_level(GraphOptimizationLevel::Level3)
                    .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?
                    .commit_from_file(model_path)
                    .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?
            }
        };

        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?;

        Ok(Self {
            id: format!("onnx-dense:{model_id}"),
            model_id: model_id.to_string(),
            session: std::sync::Mutex::new(session),
            tokenizer,
            dimensions,
        })
    }

    fn encode_text(&self, text: &str) -> Result<Vec<f32>, VectoriserError> {
        let encoding = self.tokenizer.encode(text, true)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        // Truncate to model's max sequence length (512 for BGE/MiniLM models)
        let max_len = 512;
        let ids = encoding.get_ids();
        let mask = encoding.get_attention_mask();
        let len = ids.len().min(max_len);
        let input_ids: Vec<i64> = ids[..len].iter().map(|&id| id as i64).collect();
        let attention_mask: Vec<i64> = mask[..len].iter().map(|&m| m as i64).collect();
        let seq_len = input_ids.len();

        let id_tensor = TensorRef::from_array_view(([1, seq_len], &*input_ids))
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;
        let mask_tensor = TensorRef::from_array_view(([1, seq_len], &*attention_mask))
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        // Always provide token_type_ids (zeros) — required by models like BGE,
        // harmless for models that don't use it (ort ignores unused inputs)
        let token_type_ids: Vec<i64> = vec![0i64; seq_len];
        let type_tensor = TensorRef::from_array_view(([1, seq_len], &*token_type_ids))
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let mut session = self.session.lock()
            .map_err(|e| VectoriserError::EncodeFailed(format!("session lock poisoned: {e}")))?;
        let outputs = session.run(ort::inputs![
            "input_ids" => id_tensor,
            "attention_mask" => mask_tensor,
            "token_type_ids" => type_tensor,
        ]).map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let (shape, data) = outputs[0].try_extract_tensor::<f32>()
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        // Shape derefs to [i64]
        let dims_slice: Vec<usize> = shape.iter().map(|&d| d as usize).collect();

        let vector = if dims_slice.len() == 3 {
            // (batch, seq_len, dims) — mean pool
            let token_count = dims_slice[1];
            let dims = dims_slice[2];
            let mut pooled = vec![0.0f32; dims];
            for t in 0..token_count {
                for d in 0..dims {
                    pooled[d] += data[t * dims + d];
                }
            }
            let tc = token_count as f32;
            for d in &mut pooled { *d /= tc; }
            pooled
        } else if dims_slice.len() == 2 {
            let dims = dims_slice[1];
            data[..dims].to_vec()
        } else {
            return Err(VectoriserError::EncodeFailed(format!("unexpected output shape: {dims_slice:?}")));
        };

        // L2 normalise
        let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            Ok(vector.into_iter().map(|x| x / norm).collect())
        } else {
            Ok(vector)
        }
    }
}

#[cfg(feature = "onnx")]
#[async_trait]
impl Vectoriser for OnnxDenseVectoriser {
    fn id(&self) -> &str { &self.id }
    fn model_id(&self) -> &str { &self.model_id }
    fn output_size(&self) -> usize { self.dimensions }
    fn output_kind(&self) -> VectorKind { VectorKind::Dense }

    async fn encode(&self, fields: &FieldSet) -> Result<VectorData, VectoriserError> {
        let mut pairs: Vec<_> = fields.iter().collect();
        pairs.sort_by_key(|(k, _)| (*k).clone());
        let text: String = pairs.iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(". ");
        Ok(VectorData::Dense(self.encode_text(&text)?))
    }

    async fn encode_batch(&self, batch: &[FieldSet]) -> Result<Vec<VectorData>, VectoriserError> {
        if batch.is_empty() { return Ok(Vec::new()); }

        let max_len = 512;

        // Tokenize all texts
        let texts: Vec<String> = batch.iter().map(|fields| {
            let mut pairs: Vec<_> = fields.iter().collect();
            pairs.sort_by_key(|(k, _)| (*k).clone());
            pairs.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(". ")
        }).collect();

        let encodings: Vec<_> = texts.iter().map(|t| {
            self.tokenizer.encode(t.as_str(), true)
                .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))
        }).collect::<Result<Vec<_>, _>>()?;

        // Find max sequence length in batch (capped at 512)
        let max_seq = encodings.iter().map(|e| e.get_ids().len().min(max_len)).max().unwrap_or(1);
        let batch_size = encodings.len();

        // Pad all sequences to max_seq and flatten into [batch_size * max_seq]
        let mut all_ids = vec![0i64; batch_size * max_seq];
        let mut all_mask = vec![0i64; batch_size * max_seq];
        let mut all_types = vec![0i64; batch_size * max_seq];

        for (i, enc) in encodings.iter().enumerate() {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let len = ids.len().min(max_len);
            for j in 0..len {
                all_ids[i * max_seq + j] = ids[j] as i64;
                all_mask[i * max_seq + j] = mask[j] as i64;
            }
        }

        let id_tensor = TensorRef::from_array_view(([batch_size, max_seq], &*all_ids))
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;
        let mask_tensor = TensorRef::from_array_view(([batch_size, max_seq], &*all_mask))
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;
        let type_tensor = TensorRef::from_array_view(([batch_size, max_seq], &*all_types))
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let mut session = self.session.lock()
            .map_err(|e| VectoriserError::EncodeFailed(format!("session lock poisoned: {e}")))?;
        let outputs = session.run(ort::inputs![
            "input_ids" => id_tensor,
            "attention_mask" => mask_tensor,
            "token_type_ids" => type_tensor,
        ]).map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let (shape, data) = outputs[0].try_extract_tensor::<f32>()
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;
        let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();

        // Extract per-entity vectors from batch output
        let mut results = Vec::with_capacity(batch_size);
        if dims.len() == 3 {
            // (batch, seq_len, hidden_dim) — mean pool per entity using attention mask
            let seq = dims[1];
            let hidden = dims[2];
            for i in 0..batch_size {
                let enc_len = encodings[i].get_ids().len().min(max_len);
                let mut pooled = vec![0.0f32; hidden];
                for t in 0..enc_len {
                    for d in 0..hidden {
                        pooled[d] += data[i * seq * hidden + t * hidden + d];
                    }
                }
                let tc = enc_len as f32;
                for d in &mut pooled { *d /= tc; }
                // L2 normalise
                let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 { for d in &mut pooled { *d /= norm; } }
                results.push(VectorData::Dense(pooled));
            }
        } else if dims.len() == 2 {
            // (batch, hidden_dim) — already pooled
            let hidden = dims[1];
            for i in 0..batch_size {
                let mut vec = data[i * hidden..(i + 1) * hidden].to_vec();
                let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 { for d in &mut vec { *d /= norm; } }
                results.push(VectorData::Dense(vec));
            }
        } else {
            return Err(VectoriserError::EncodeFailed(format!("unexpected batch output shape: {dims:?}")));
        }

        Ok(results)
    }

    async fn encode_query(&self, query: &str) -> Result<VectorData, VectoriserError> {
        Ok(VectorData::Dense(self.encode_text(query)?))
    }
}

#[cfg(all(test, feature = "onnx"))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    #[ignore] // Requires model files
    async fn onnx_dense_encode() {
        let model_path = PathBuf::from(std::env::var("TRONDB_TEST_MODEL_PATH").unwrap());
        let tokenizer_path = PathBuf::from(std::env::var("TRONDB_TEST_TOKENIZER_PATH").unwrap());
        let v = OnnxDenseVectoriser::new("bge-small-en-v1.5", &model_path, &tokenizer_path, 384).unwrap();
        let fields = FieldSet::from([("name".into(), "Jazz Club".into())]);
        let result = v.encode(&fields).await.unwrap();
        match result {
            VectorData::Dense(d) => {
                assert_eq!(d.len(), 384);
                let norm: f32 = d.iter().map(|x| x * x).sum::<f32>().sqrt();
                assert!((norm - 1.0).abs() < 0.01);
            }
            _ => panic!("expected Dense"),
        }
    }
}
