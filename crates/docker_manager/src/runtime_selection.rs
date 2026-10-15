//! Runtime type detection based on environment variables
//!
//! This module provides `RuntimeType` enum and detection logic to switch
//! between Docker and Kubernetes container runtimes at runtime.

use std::env;
use tracing::info;

/// Runtime type selection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeType {
    /// Docker runtime (default)
    Docker,
    /// Kubernetes runtime
    Kubernetes,
}

impl RuntimeType {
    /// Detect runtime type from environment variable
    ///
    /// Supported values:
    /// - "docker" or not set -> Docker runtime
    /// - "kubernetes" or "k8s" -> Kubernetes runtime
    pub fn from_env() -> Self {
        match env::var("CONTAINER_RUNTIME").as_deref() {
            Ok("kubernetes") | Ok("k8s") => {
                info!("[RUNTIME] Using Kubernetes runtime (from CONTAINER_RUNTIME)");
                RuntimeType::Kubernetes
            }
            Ok("docker") | Ok("") | Err(_) => {
                info!("[RUNTIME] Using Docker runtime (from CONTAINER_RUNTIME or default)");
                RuntimeType::Docker
            }
            Ok(other) => {
                info!(
                    "[RUNTIME] Unknown CONTAINER_RUNTIME '{}', falling back to Docker",
                    other
                );
                RuntimeType::Docker
            }
        }
    }

    /// Check if Kubernetes runtime is enabled
    #[cfg(feature = "kubernetes")]
    pub fn is_kubernetes(&self) -> bool {
        matches!(self, RuntimeType::Kubernetes)
    }

    #[cfg(not(feature = "kubernetes"))]
    pub fn is_kubernetes(&self) -> bool {
        false
    }
}

// 豁免仅限测试模块：edition-2024 的 env 变异（set_var/remove_var）是 unsafe，
// 测试内有 ENV_MUTEX 串行化保护（各处带 SAFETY 注释）
#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // 使用全局互斥锁确保环境变量测试串行执行
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_runtime_type_from_env_docker() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: Test-only environment variable manipulation, protected by mutex
        unsafe {
            env::set_var("CONTAINER_RUNTIME", "docker");
        }
        assert_eq!(RuntimeType::from_env(), RuntimeType::Docker);
        // SAFETY: Test-only environment variable cleanup, protected by mutex
        unsafe {
            env::remove_var("CONTAINER_RUNTIME");
        }
    }

    #[test]
    fn test_runtime_type_from_env_kubernetes() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: Test-only environment variable manipulation, protected by mutex
        unsafe {
            env::set_var("CONTAINER_RUNTIME", "kubernetes");
        }
        assert_eq!(RuntimeType::from_env(), RuntimeType::Kubernetes);
        // SAFETY: Test-only environment variable cleanup, protected by mutex
        unsafe {
            env::remove_var("CONTAINER_RUNTIME");
        }
    }

    #[test]
    fn test_runtime_type_from_env_k8s() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: Test-only environment variable manipulation, protected by mutex
        unsafe {
            env::set_var("CONTAINER_RUNTIME", "k8s");
        }
        assert_eq!(RuntimeType::from_env(), RuntimeType::Kubernetes);
        // SAFETY: Test-only environment variable cleanup, protected by mutex
        unsafe {
            env::remove_var("CONTAINER_RUNTIME");
        }
    }

    #[test]
    fn test_runtime_type_from_env_default() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: Test-only environment variable cleanup, protected by mutex
        unsafe {
            env::remove_var("CONTAINER_RUNTIME");
        }
        assert_eq!(RuntimeType::from_env(), RuntimeType::Docker);
    }

    #[test]
    fn test_runtime_type_from_env_unknown() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: Test-only environment variable manipulation, protected by mutex
        unsafe {
            env::set_var("CONTAINER_RUNTIME", "unknown");
        }
        assert_eq!(RuntimeType::from_env(), RuntimeType::Docker);
        // SAFETY: Test-only environment variable cleanup, protected by mutex
        unsafe {
            env::remove_var("CONTAINER_RUNTIME");
        }
    }
}
