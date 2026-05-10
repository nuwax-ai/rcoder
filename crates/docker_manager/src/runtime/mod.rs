//! Runtime abstraction module
//!
//! This module provides container runtime abstraction to support both
//! Docker and Kubernetes backends.

pub mod docker_runtime;
pub mod kubernetes_runtime;
pub mod manager;

pub use docker_runtime::DockerRuntime;
#[cfg(feature = "kubernetes")]
pub use kubernetes_runtime::KubernetesRuntime;
pub use manager::RuntimeManager;
