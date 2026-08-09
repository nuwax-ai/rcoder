//! Container Runtime API
//!
//! This crate provides the `ContainerRuntime` trait abstraction for different
//! container runtimes (Docker, Kubernetes, etc.).

pub mod container_params;
pub mod runtime_trait;
pub mod types;
pub mod utils;

// re-export（保持外部引用不变：`container_runtime_api::DeploymentStatus` 等）
pub use container_params::*;
pub use runtime_trait::*;
pub use types::*;
pub use utils::*;
