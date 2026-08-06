//! 测试支撑：MockRuntime（UserAppRuntime 假实现）+ AppService 直构造（仅 cfg(test)）。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use docker_manager::path::HostPathResolver;
use moka::sync::Cache;

use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, DeploymentStatus,
    UserAppDeploymentRuntime, UserAppRuntime, WorkspaceRuntime,
};
use shared_types::ContainerBasicInfo;

use crate::activity_registry::AppActivityRegistry;
use crate::config::{AppAccessMode, AppManagerConfig};
use crate::service::AppService;

/// 可控假运行时：记录 delete/create 调用次数，可开关失败注入。
///
/// WorkspaceRuntime 全走默认实现（resolve_workspace_path → Ok(None)，
/// get_container_app_dir 因此落到 `workspace_root/{app_id}`，测试用 tempdir 承接）。
#[derive(Default)]
pub(crate) struct MockRuntime {
    pub delete_calls: AtomicUsize,
    pub delete_fails: AtomicBool,
    pub create_calls: AtomicUsize,
    pub create_fails: AtomicBool,
}

#[async_trait]
impl WorkspaceRuntime for MockRuntime {}

#[async_trait]
impl UserAppDeploymentRuntime for MockRuntime {
    async fn get_deployment_status(
        &self,
        _app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        // 一律“不存在”：start/stop 路径得到 NotFound（confirm/stop 均容忍）
        Ok(None)
    }

    async fn delete_deployment(&self, _app_id: &str) -> ContainerRuntimeResult<()> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        if self.delete_fails.load(Ordering::SeqCst) {
            Err(ContainerRuntimeError::DockerError(
                "mock delete_deployment failure".into(),
            ))
        } else {
            Ok(())
        }
    }

    async fn create_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        if self.create_fails.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ContainerCreationError(
                "mock create_deployment failure".into(),
            ));
        }
        let project_id = params.project_id.unwrap_or_default();
        Ok(ContainerBasicInfo {
            container_id: "mock-container-id".into(),
            container_name: format!("userapp-{project_id}"),
            container_ip: "10.0.0.1".into(),
            internal_port: 0,
            external_port: 0,
            project_id,
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: String::new(),
        })
    }
}

/// 直构造 AppService（绕过 `new` 的 HostPathResolver/K8s 前置校验副作用），
/// Docker 模式 + 指定 workspace_root（测试 tempdir）。
pub(crate) fn test_service(workspace_root: &Path, runtime: Arc<MockRuntime>) -> AppService {
    let config = AppManagerConfig {
        workspace_root: Some(workspace_root.to_string_lossy().into_owned()),
        access_mode: AppAccessMode::Docker,
        ..AppManagerConfig::default()
    };
    let path_resolver: Cache<String, Arc<HostPathResolver>> =
        Cache::builder().max_capacity(1).build();
    AppService {
        config,
        runtime: runtime as Arc<dyn UserAppRuntime>,
        activity: Arc::new(AppActivityRegistry::new(Duration::from_secs(300))),
        pingora: None,
        path_resolver,
        pingora_ports: DashMap::new(),
        release_locks: DashMap::new(),
    }
}
