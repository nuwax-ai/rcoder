//! Runtime manager implementation
//!
//! This module provides `RuntimeManager` that selects and manages the appropriate
//! container runtime (Docker or Kubernetes) based on environment configuration.

use container_runtime_api::ContainerRuntime;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tracing::{error, info};

use crate::runtime_selection::RuntimeType;
#[cfg(feature = "kubernetes")]
use super::KubernetesRuntime;
use super::DockerRuntime;
use crate::types::DockerManagerConfig;

/// Global runtime instance
static RUNTIME_INSTANCE: OnceCell<Arc<dyn ContainerRuntime>> = OnceCell::const_new();

/// Runtime manager that selects and manages the appropriate container runtime
pub struct RuntimeManager;

impl RuntimeManager {
    /// Initialize the global runtime based on environment configuration
    pub async fn init(
        config: DockerManagerConfig,
    ) -> container_runtime_api::ContainerRuntimeResult<()> {
        let runtime_type = RuntimeType::from_env();

        let runtime: Arc<dyn ContainerRuntime> = match runtime_type {
            RuntimeType::Docker => {
                info!("[RUNTIME] Initializing Docker runtime");
                let docker_manager = crate::DockerManager::new(config.clone())
                    .await
                    .map_err(|e| {
                        container_runtime_api::ContainerRuntimeError::ConnectionError(
                            e.to_string(),
                        )
                    })?;
                Arc::new(DockerRuntime::new(Arc::new(docker_manager)))
            }
            #[cfg(feature = "kubernetes")]
            RuntimeType::Kubernetes => {
                info!("[RUNTIME] Initializing Kubernetes runtime");
                let k8s_runtime = KubernetesRuntime::new(config.clone())
                    .await
                    .map_err(|e| {
                        container_runtime_api::ContainerRuntimeError::K8sError(e.to_string())
                    })?;
                Arc::new(k8s_runtime)
            }
            #[cfg(not(feature = "kubernetes"))]
            RuntimeType::Kubernetes => {
                return Err(
                    container_runtime_api::ContainerRuntimeError::ConfigurationError(
                        "Kubernetes runtime requested but 'kubernetes' feature is not enabled."
                            .to_string(),
                    ),
                );
            }
        };

        // Verify runtime health
        runtime.health_check().await.map_err(|e| {
            error!("[RUNTIME] Health check failed: {}", e);
            e
        })?;

        RUNTIME_INSTANCE.set(runtime).map_err(|_| {
            container_runtime_api::ContainerRuntimeError::ConfigurationError(
                "Runtime already initialized".to_string(),
            )
        })?;

        info!("[RUNTIME] Global runtime initialized successfully");
        Ok(())
    }

    /// Get the global runtime instance
    pub async fn get(
    ) -> container_runtime_api::ContainerRuntimeResult<Arc<dyn ContainerRuntime>> {
        RUNTIME_INSTANCE.get().cloned().ok_or_else(|| {
            container_runtime_api::ContainerRuntimeError::ConfigurationError(
                "Runtime not initialized. Call RuntimeManager::init() first.".to_string(),
            )
        })
    }

    /// Check if Docker runtime is in use
    pub fn is_docker() -> bool {
        RuntimeType::from_env() == RuntimeType::Docker
    }

    /// Check if Kubernetes runtime is in use
    #[cfg(feature = "kubernetes")]
    pub fn is_kubernetes() -> bool {
        RuntimeType::from_env() == RuntimeType::Kubernetes
    }

    #[cfg(not(feature = "kubernetes"))]
    pub fn is_kubernetes() -> bool {
        false
    }

    /// Get the current runtime type
    pub fn runtime_type() -> RuntimeType {
        RuntimeType::from_env()
    }
}
