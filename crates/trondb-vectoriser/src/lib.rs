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
