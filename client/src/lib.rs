pub mod api;
pub mod archive;
pub mod bundle;
pub mod classify;
pub mod cli;
pub mod config;
pub mod convert;
pub mod dicom;
pub mod model;
pub mod pipeline;
mod privacy;
pub mod progress;
pub mod pseudonym;
pub mod s3;
pub mod state;
pub mod terminal;

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SIDECAR_SCHEMA_VERSION: &str = "1.0.0";
pub const MANIFEST_SCHEMA_VERSION: &str = "3.0.0";
/// Bump only when classification decisions or evidence semantics change.
pub const DICOM_CLASSIFIER_CONTRACT_VERSION: &str = "2.0.0";
/// Bump only when deterministic archive bytes/identity semantics change.
pub const DICOM_ARCHIVE_CONTRACT_VERSION: &str = "2.0.0";
pub const PINNED_DCM2NIIX_VERSION: &str = "v1.0.20260416";
pub const DEFAULT_API_URL: &str = "https://scalingneuro.com";
