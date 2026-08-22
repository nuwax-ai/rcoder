//! Runtime abstraction module
//!
//! This module provides container runtime abstraction to support both
//! Docker and Kubernetes backends.

pub(crate) mod docker_app_runtime;
pub mod docker_runtime;
pub(crate) mod k8s_runtime_helpers;
pub mod kubernetes_runtime;
pub mod manager;

#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_agent_create;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_agent_pod;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_agent_query;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_app_create;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_app_helpers;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_app_lifecycle;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_app_observation;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_app_query;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_deployment;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_pod;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_pvc;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_service;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_statefulset;
#[cfg(feature = "kubernetes")]
pub(crate) mod k8s_translate;

pub use docker_runtime::DockerRuntime;
#[cfg(feature = "kubernetes")]
pub use kubernetes_runtime::KubernetesRuntime;
pub use manager::RuntimeManager;
