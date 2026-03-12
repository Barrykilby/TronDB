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
pub struct OnnxDenseVectoriser {
    id: String,
    model_id: String,
    session: Session,
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
        let session = Session::builder()
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?
            .commit_from_file(model_path)
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?;

        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| VectoriserError::ModelNotLoaded(e.to_string()))?;

        Ok(Self {
            id: format!("onnx-dense:{model_id}"),
            model_id: model_id.to_string(),
            session,
            tokenizer,
            dimensions,
        })
    }

    fn encode_text(&self, text: &str) -> Result<Vec<f32>, VectoriserError> {
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

        let embeddings = outputs[0].try_extract_tensor::<f32>()
            .map_err(|e| VectoriserError::EncodeFailed(e.to_string()))?;

        let view = embeddings.view();
        let shape = view.shape();

        let vector = if shape.len() == 3 {
            // (batch, seq_len, dims) — mean pool
            let dims = shape[2];
            let mut pooled = vec![0.0f32; dims];
            let token_count = shape[1] as f32;
            for t in 0..shape[1] {
                for d in 0..dims {
                    pooled[d] += view[[0, t, d]];
                }
            }
            for d in &mut pooled { *d /= token_count; }
            pooled
        } else if shape.len() == 2 {
            view.row(0).to_vec()
        } else {
            return Err(VectoriserError::EncodeFailed(format!("unexpected output shape: {shape:?}")));
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
