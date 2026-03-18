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
const MIN_WEIGHT: f32 = 0.001;

#[cfg(feature = "onnx")]
pub struct OnnxSparseVectoriser {
    id: String,
    model_id: String,
    session: std::sync::Mutex<Session>,
    tokenizer: Tokenizer,
    vocab_size: usize,
}

#[cfg(feature = "onnx")]
impl OnnxSparseVectoriser {
    pub fn new(
        model_id: &str,
        model_path: &Path,
        tokenizer_path: &Path,
        vocab_size: usize,
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
                    tracing::info!(model = model_id, "ONNX sparse vectoriser using CUDA");
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
            id: format!("onnx-sparse:{model_id}"),
            model_id: model_id.to_string(),
            session: std::sync::Mutex::new(session),
            tokenizer,
            vocab_size,
        })
    }

    fn encode_text(&self, text: &str) -> Result<Vec<(u32, f32)>, VectoriserError> {
        let encoding = self.tokenizer.encode(text, true)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;
        // Truncate to model's max sequence length
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

        let mut session = self.session.lock()
            .map_err(|e| VectoriserError::EncodeFailed(format!("session lock poisoned: {e}")))?;
        let outputs = session.run(ort::inputs![
            "input_ids" => id_tensor,
            "attention_mask" => mask_tensor,
        ]).map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let (shape, data) = outputs[0].try_extract_tensor::<f32>()
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        // Shape derefs to [i64]
        let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();

        // ReLU + log1p, then max-pool over tokens
        let vocab_dim = if dims.len() == 3 { dims[2] } else { dims[1] };
        let mut pooled = vec![0.0f32; vocab_dim];

        if dims.len() == 3 {
            let token_count = dims[1];
            for t in 0..token_count {
                for d in 0..vocab_dim {
                    let val = data[t * vocab_dim + d].max(0.0_f32); // ReLU
                    let val = (1.0_f32 + val).ln(); // log1p
                    pooled[d] = pooled[d].max(val); // max-pool
                }
            }
        } else {
            for d in 0..vocab_dim {
                let val = data[d].max(0.0_f32);
                pooled[d] = (1.0_f32 + val).ln();
            }
        }

        // Extract non-zero entries
        let sparse: Vec<(u32, f32)> = pooled.iter().enumerate()
            .filter(|(_, &w)| w > MIN_WEIGHT)
            .map(|(i, &w)| (i as u32, w))
            .collect();

        Ok(sparse)
    }
}

#[cfg(feature = "onnx")]
#[async_trait]
impl Vectoriser for OnnxSparseVectoriser {
    fn id(&self) -> &str { &self.id }
    fn model_id(&self) -> &str { &self.model_id }
    fn output_size(&self) -> usize { self.vocab_size }
    fn output_kind(&self) -> VectorKind { VectorKind::Sparse }

    async fn encode(&self, fields: &FieldSet) -> Result<VectorData, VectoriserError> {
        let mut pairs: Vec<_> = fields.iter().collect();
        pairs.sort_by_key(|(k, _)| (*k).clone());
        let text: String = pairs.iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(". ");
        Ok(VectorData::Sparse(self.encode_text(&text)?))
    }

    async fn encode_query(&self, query: &str) -> Result<VectorData, VectoriserError> {
        Ok(VectorData::Sparse(self.encode_text(query)?))
    }
}
