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

/// Create a vectoriser instance from per-representation and/or collection-level config.
///
/// Resolution: `repr_config` fields take priority, gaps filled from `fallback_config`.
/// Selects the appropriate backend (external, network, ONNX dense/sparse) based on
/// `vectoriser_type` and the representation's properties.
pub fn create_vectoriser_from_config(
    repr_config: Option<&trondb_core::types::VectoriserConfig>,
    fallback_config: Option<&trondb_core::types::VectoriserConfig>,
    repr: &trondb_core::types::StoredRepresentation,
) -> Result<Arc<dyn Vectoriser>, VectoriserError> {
    // Merge: repr_config fields preferred, fallback fills gaps
    let resolved = merge_configs(repr_config, fallback_config);
    let config = &resolved;
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

/// Merge two optional VectoriserConfigs. `primary` fields take precedence; `fallback` fills gaps.
fn merge_configs(
    primary: Option<&trondb_core::types::VectoriserConfig>,
    fallback: Option<&trondb_core::types::VectoriserConfig>,
) -> trondb_core::types::VectoriserConfig {
    let empty = trondb_core::types::VectoriserConfig {
        model: None, model_path: None, device: None,
        vectoriser_type: None, endpoint: None, auth: None,
    };
    let p = primary.unwrap_or(&empty);
    let f = fallback.unwrap_or(&empty);
    trondb_core::types::VectoriserConfig {
        model: p.model.clone().or_else(|| f.model.clone()),
        model_path: p.model_path.clone().or_else(|| f.model_path.clone()),
        device: p.device.clone().or_else(|| f.device.clone()),
        vectoriser_type: p.vectoriser_type.clone().or_else(|| f.vectoriser_type.clone()),
        endpoint: p.endpoint.clone().or_else(|| f.endpoint.clone()),
        auth: p.auth.clone().or_else(|| f.auth.clone()),
    }
}
