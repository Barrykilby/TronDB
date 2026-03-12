pub mod passthrough;
pub mod mock;

// Feature-gated modules
#[cfg(feature = "onnx")]
pub mod onnx_dense;
#[cfg(feature = "onnx")]
pub mod onnx_sparse;

pub mod network;

#[cfg(feature = "external")]
pub mod external;

pub mod recipe;

// Re-exports for convenience
pub use passthrough::PassthroughVectoriser;
pub use mock::MockVectoriser;
pub use network::NetworkVectoriser;
#[cfg(feature = "external")]
pub use external::ExternalVectoriser;

use std::sync::Arc;
use trondb_core::vectoriser::{Vectoriser, VectoriserError};

/// Create a vectoriser instance from a collection's persisted configuration.
///
/// Selects the appropriate backend (external, network, ONNX dense/sparse) based on
/// `VectoriserConfig::vectoriser_type` and the representation's properties.
pub fn create_vectoriser_from_config(
    config: &trondb_core::types::VectoriserConfig,
    repr: &trondb_core::types::StoredRepresentation,
) -> Result<Arc<dyn Vectoriser>, VectoriserError> {
    match config.vectoriser_type.as_deref() {
        Some("external") => {
            #[cfg(feature = "external")]
            {
                let endpoint = config.endpoint.as_deref()
                    .ok_or(VectoriserError::ModelNotLoaded("external vectoriser requires ENDPOINT".into()))?;
                let auth = config.auth.as_deref().unwrap_or("");
                let model = config.model.as_deref().unwrap_or("unknown");
                let dims = repr.dimensions.unwrap_or(0);
                Ok(Arc::new(external::ExternalVectoriser::new(model, endpoint, auth, dims)))
            }
            #[cfg(not(feature = "external"))]
            Err(VectoriserError::NotSupported("external feature not enabled".into()))
        }
        Some("network") => {
            let endpoint = config.endpoint.as_deref()
                .ok_or(VectoriserError::ModelNotLoaded("network vectoriser requires ENDPOINT".into()))?;
            let model = config.model.as_deref().unwrap_or("unknown");
            let dims = repr.dimensions.unwrap_or(0);
            Ok(Arc::new(network::NetworkVectoriser::new(model, endpoint, dims)))
        }
        _ => {
            #[cfg(feature = "onnx")]
            {
                let model_path = config.model_path.as_deref()
                    .ok_or(VectoriserError::ModelNotLoaded("local vectoriser requires MODEL_PATH".into()))?;
                let model = config.model.as_deref().unwrap_or("unknown");
                let dims = repr.dimensions.unwrap_or(0);
                let model_dir = std::path::Path::new(model_path).parent().unwrap_or(std::path::Path::new("."));
                let tokenizer_path = model_dir.join("tokenizer.json");
                if repr.sparse {
                    Ok(Arc::new(onnx_sparse::OnnxSparseVectoriser::new(
                        model, std::path::Path::new(model_path), &tokenizer_path, dims
                    )?))
                } else {
                    Ok(Arc::new(onnx_dense::OnnxDenseVectoriser::new(
                        model, std::path::Path::new(model_path), &tokenizer_path, dims
                    )?))
                }
            }
            #[cfg(not(feature = "onnx"))]
            Err(VectoriserError::NotSupported(
                "onnx feature not enabled — cannot create local vectoriser".into()
            ))
        }
    }
}
