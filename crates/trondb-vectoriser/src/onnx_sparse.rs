#![allow(unused_imports)]
use async_trait::async_trait;
use std::path::Path;

use trondb_core::types::VectorData;
use trondb_core::vectoriser::{FieldSet, VectorKind, Vectoriser, VectoriserError};

#[cfg(feature = "onnx")]
use ort::Session;
#[cfg(feature = "onnx")]
use tokenizers::Tokenizer;

#[cfg(feature = "onnx")]
const MIN_WEIGHT: f32 = 0.001;

#[cfg(feature = "onnx")]
pub struct OnnxSparseVectoriser {
    id: String,
    model_id: String,
    session: Session,
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
        let session = Session::builder()
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?
            .commit_from_file(model_path)
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?;
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?;
        Ok(Self {
            id: format!("onnx-sparse:{model_id}"),
            model_id: model_id.to_string(),
            session,
            tokenizer,
            vocab_size,
        })
    }

    fn encode_text(&self, text: &str) -> Result<Vec<(u32, f32)>, VectoriserError> {
        let encoding = self.tokenizer.encode(text, true)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;
        let input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let attention_mask: Vec<i64> = encoding.get_attention_mask().iter().map(|&m| m as i64).collect();
        let seq_len = input_ids.len();

        let input_ids_array = ndarray::Array2::from_shape_vec((1, seq_len), input_ids)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;
        let attention_mask_array = ndarray::Array2::from_shape_vec((1, seq_len), attention_mask)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let outputs = self.session.run(ort::inputs![
            "input_ids" => input_ids_array,
            "attention_mask" => attention_mask_array,
        ].map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?)
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let logits = outputs[0].try_extract_tensor::<f32>()
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;
        let view = logits.view();
        let shape = view.shape();

        // ReLU + log1p, then max-pool over tokens
        let vocab_dim = if shape.len() == 3 { shape[2] } else { shape[1] };
        let mut pooled = vec![0.0f32; vocab_dim];

        if shape.len() == 3 {
            for t in 0..shape[1] {
                for d in 0..vocab_dim {
                    let val = view[[0, t, d]].max(0.0); // ReLU
                    let val = (1.0 + val).ln(); // log1p
                    pooled[d] = pooled[d].max(val); // max-pool
                }
            }
        } else {
            for d in 0..vocab_dim {
                let val = view[[0, d]].max(0.0);
                pooled[d] = (1.0 + val).ln();
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
