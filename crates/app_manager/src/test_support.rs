//! 测试支撑：MockRuntime（UserAppRuntime 假实现）+ AppService 直构造（仅 cfg(test)）。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;

use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, ContainerSpecSnapshot,
    DeploymentStatus, StorageResizeOutcome, UserAppDeploymentRuntime, UserAppRuntime,
    WorkspaceRuntime,
};
use shared_types::{ContainerBasicInfo, ServiceType};

use crate::activity_registry::AppActivityRegistry;
use crate::config::{AppAccessMode, AppManagerConfig};
use crate::service::AppService;

/// 可控假运行时：记录 delete/create 调用次数，可开关失败注入。
///
/// WorkspaceRuntime 全走默认实现（resolve_workspace_path → Ok(None)，
/// get_container_app_dir 因此落到 `workspace_root/{app_id}`，测试用 tempdir 承接）。
/// `specs` 预置 app 的 live desired 快照（get_app_container_spec 回退测试用；缺省空快照）。
/// `deployments` 预置 app 运行时状态（query_apps/list 过滤测试用；缺省空列表）。
/// `status_fails` 注入 get_deployment_status 的瞬时后端错误（wait_app_ready 容错测试）。
#[derive(Default)]
pub(crate) struct MockRuntime {
    pub delete_calls: AtomicUsize,
    pub delete_fails: AtomicBool,
    /// list_deployments 穿透计数（查询缓存测试用）
    pub list_calls: AtomicUsize,
    pub create_calls: AtomicUsize,
    pub create_fails: AtomicBool,
    pub status_fails: AtomicUsize,
    /// start_app（scale>0）后 phase 停在 Error：模拟新版本启动即崩（就绪失败测试）。
    pub crash_on_start: AtomicBool,
    pub specs: DashMap<String, ContainerSpecSnapshot>,
    pub deployments: DashMap<String, DeploymentStatus>,
    /// create/patch 收到的参数调用历史（key=project_id 按序追加；断言取首次创建
    /// 参数用——update 通道的 re-apply 会以 live 回退值再次进入本方法）
    pub create_params_history: DashMap<String, Vec<ContainerCreateParams>>,
    /// resize_app_storage 收到的目标值历史（key=app_id 按序追加；断言 update 是否
    /// 触发扩容/传值）。
    pub resize_calls: DashMap<String, Vec<String>>,
    /// 注入 resize_app_storage 失败（true → ConnectionError → update 应整体失败）。
    pub resize_fails: AtomicBool,
    /// 注入 resize_app_storage 返回 outcome（None → 默认模拟 K8s Grow 成功）。
    pub resize_outcome: std::sync::Mutex<Option<StorageResizeOutcome>>,
    /// workspace_volume_name 收到的 (app_id, service_type debug) 历史——storage
    /// env 分派断言用（dev→UserAppBuilder / prod→UserApp）。
    pub volume_name_calls: DashMap<String, Vec<String>>,
    /// list_workspace_identifiers 按 service_type 的返回预置 + 调用计数
    /// （storage get/query 的 env 分派断言用）。
    pub workspace_ids: DashMap<String, Vec<String>>,
    pub list_workspace_calls: AtomicUsize,
}

#[async_trait]
impl WorkspaceRuntime for MockRuntime {
    // 其余 workspace 族方法走默认实现（resolve_workspace_path → Ok(None)，
    // get_container_app_dir 因此落到 `workspace_root/{app_id}`，测试用 tempdir 承接）；
    // 覆写 resize_app_storage（update 扩容链路断言需要记录与注入）与
    // workspace_volume_name / list_workspace_identifiers（storage env 分派断言）。
    async fn workspace_volume_name(
        &self,
        app_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        self.volume_name_calls
            .entry(app_id.to_string())
            .or_default()
            .push(format!("{service_type:?}"));
        Ok(format!("vol-{app_id}"))
    }

    async fn list_workspace_identifiers(
        &self,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Vec<String>> {
        self.list_workspace_calls.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .workspace_ids
            .get(&format!("{service_type:?}"))
            .map(|v| v.clone())
            .unwrap_or_default())
    }

    async fn resize_app_storage(
        &self,
        app_id: &str,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome> {
        self.resize_calls
            .entry(app_id.to_string())
            .or_default()
            .push(new_size.to_string());
        if self.resize_fails.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ConnectionError(
                "mock resize_app_storage failure".into(),
            ));
        }
        Ok(self
            .resize_outcome
            .lock()
            .expect("resize_outcome lock")
            .clone()
            .unwrap_or_else(|| StorageResizeOutcome::Resized {
                from: "100Gi".into(),
                to: new_size.to_string(),
            }))
    }
}

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

    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        self.list_calls.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .deployments
            .iter()
            .map(|entry| entry.value().clone())
            .collect())
    }

    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        // 注入的瞬时后端错误（模拟 API 抖动/网络瞬断），扣减后恢复
        let pending = self.status_fails.load(Ordering::SeqCst);
        if pending > 0 {
            self.status_fails.store(pending - 1, Ordering::SeqCst);
            return Err(ContainerRuntimeError::ConnectionError(
                "injected transient failure".to_string(),
            ));
        }
        // 优先查预置 deployments（activate/rollback 测试）；未预置 → None
        // （start/stop 路径得到 NotFound，相关清理路径容忍）
        Ok(self
            .deployments
            .get(app_id)
            .map(|entry| entry.value().clone()))
    }

    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        if let Some(mut entry) = self.deployments.get_mut(app_id) {
            let status = entry.value_mut();
            status.replicas = replicas;
            status.ready_replicas = replicas.max(0);
            status.phase = if replicas == 0 {
                "Stopped"
            } else if self.crash_on_start.load(Ordering::SeqCst) {
                // 注入"启动即崩"：start_app 后 phase=Error，activate 的 wait_app_ready
                // 首个轮询即失败（无竞态地构造就绪失败场景）。
                "Error"
            } else {
                "Running"
            }
            .into();
        }
        Ok(())
    }

    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        if self.delete_fails.load(Ordering::SeqCst) {
            Err(ContainerRuntimeError::DockerError(
                "mock delete_deployment failure".into(),
            ))
        } else {
            // 与真实后端一致：删除后 status 查询 NotFound（purge 分支的
            // ensure_app_deleted 依赖此移除 deployments 条目的行为）
            self.deployments.remove(app_id);
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
        let project_id = params.project_id.clone().unwrap_or_default();
        // 捕获参数调用历史（首次创建的 ports/env 断言用）
        self.create_params_history
            .entry(project_id.clone())
            .or_default()
            .push(params.clone());
        // 登记 deployments（后续 get_app/update 流程的 fetch_runtime_status 需要）
        self.deployments
            .entry(project_id.clone())
            .or_insert_with(|| DeploymentStatus {
                app_id: project_id.clone(),
                replicas: 1,
                ready_replicas: 1,
                phase: "Running".into(),
                ..Default::default()
            });
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

    async fn patch_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // update_app 路径：与 create 同构（不注入失败；登记 deployments 供 get_app）
        self.create_deployment(params).await
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
    AppService {
        config,
        runtime: runtime as Arc<dyn UserAppRuntime>,
        activity: Arc::new(AppActivityRegistry::new(Duration::from_secs(300))),
        pingora: None,
        pingora_ports: DashMap::new(),
        release_locks: DashMap::new(),
        metadata: crate::runtime::metadata::AppMetadataStore::default(),
        dev_cleanup: std::sync::RwLock::new(None),
        dev_locator: std::sync::RwLock::new(None),
        deploy_list_cache: tokio::sync::Mutex::new(None),
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

/// 内存版 [`shared_types::AppMetadataPersistence`]（query 过滤测试注入用；
/// rows 可从测试侧直接读写断言）。
pub(crate) struct InMemoryMetadataPersistence {
    pub rows: std::sync::Mutex<Vec<shared_types::AppMetadataRecord>>,
}

impl InMemoryMetadataPersistence {
    pub fn new(rows: Vec<shared_types::AppMetadataRecord>) -> Arc<Self> {
        Arc::new(Self {
            rows: std::sync::Mutex::new(rows),
        })
    }
}

#[async_trait]
impl shared_types::AppMetadataPersistence for InMemoryMetadataPersistence {
    async fn upsert(&self, record: &shared_types::AppMetadataRecord) -> anyhow::Result<()> {
        let mut rows = self.rows.lock().expect("rows lock");
        match rows.iter_mut().find(|r| r.app_id == record.app_id) {
            Some(existing) => *existing = record.clone(),
            None => rows.push(record.clone()),
        }
        Ok(())
    }

    async fn load_all(&self) -> anyhow::Result<Vec<shared_types::AppMetadataRecord>> {
        Ok(self.rows.lock().expect("rows lock").clone())
    }

    async fn delete(&self, app_id: &str) -> anyhow::Result<()> {
        self.rows
            .lock()
            .expect("rows lock")
            .retain(|r| r.app_id != app_id);
        Ok(())
    }
}
