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
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, ContainerSpecSnapshot,
    DeploymentStatus, UserAppDeploymentRuntime, UserAppRuntime, WorkspaceRuntime,
};
use shared_types::ContainerBasicInfo;

use crate::activity_registry::AppActivityRegistry;
use crate::config::{AppAccessMode, AppManagerConfig};
use crate::service::AppService;

/// 可控假运行时：记录 delete/create 调用次数，可开关失败注入。
///
/// WorkspaceRuntime 全走默认实现（resolve_workspace_path → Ok(None)，
/// get_container_app_dir 因此落到 `workspace_root/{app_id}`，测试用 tempdir 承接）。
/// `specs` 预置 app 的 live desired 快照（get_app_container_spec 回退测试用；缺省空快照）。
#[derive(Default)]
pub(crate) struct MockRuntime {
    pub delete_calls: AtomicUsize,
    pub delete_fails: AtomicBool,
    pub create_calls: AtomicUsize,
    pub create_fails: AtomicBool,
    pub specs: DashMap<String, ContainerSpecSnapshot>,
}

#[async_trait]
impl WorkspaceRuntime for MockRuntime {}

#[async_trait]
impl UserAppDeploymentRuntime for MockRuntime {
    async fn get_app_container_spec(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<ContainerSpecSnapshot> {
        Ok(self
            .specs
            .get(app_id)
            .map(|s| s.clone())
            .unwrap_or_default())
    }

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

/// 合法 schema_version=1 release lock（build_container_params 的 inject_release_identity
/// 需要 code/release.lock.toml；service/app_params 测试共享）。
pub(crate) fn release_lock() -> &'static str {
    r#"
schema_version = 1
release_id = "release-1"
workspace_name = "smoke"
minimum_app_cli_version = "0.1.0"
runtime_image_digest = "registry.example/app-runtime:0.1.140"

[pingap]
mode = "managed"
version = "0.13.7"
commit = "abc123"

[[services]]
service_id = "backend"
name = "Backend"
dir = "backend"
type = "go"
kind = "web"
enabled = true
port = 4100
logs = []

[services.run]
command = ["./server"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[services.health]

[services.proxy]
path = "/"
strip_prefix = false
plugins = []
upstream_includes = []

[services.env]
"#
}
