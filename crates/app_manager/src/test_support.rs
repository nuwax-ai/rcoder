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
/// `status_fails` 注入 get_deployment_status 的瞬时后端错误（查询链容错测试；
/// 历史用途 wait_app_ready 已退役，现有消费者见 purge 链不缺席测试）。
#[derive(Default)]
pub(crate) struct MockRuntime {
    pub scale_calls: AtomicUsize,
    pub lease_held: Arc<AtomicBool>,
    pub env_commit_failure: AtomicUsize,
    pub patch_preparation_fails: AtomicBool,
    pub ensure_workspace_calls: AtomicUsize,
    pub delete_calls: AtomicUsize,
    pub delete_fails: AtomicBool,
    /// list_deployments 穿透计数（查询缓存测试用）
    pub list_calls: AtomicUsize,
    pub create_calls: AtomicUsize,
    pub create_fails: AtomicBool,
    pub status_fails: AtomicUsize,
    pub stop_failure_status: AtomicUsize,
    pub policy_calls: AtomicUsize,
    /// Deterministic read window for lifecycle query races; clone outside the
    /// mutex before awaiting either barrier phase.
    pub status_barrier: std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
    /// 结构化 Pod 观察 scripting（部署失败信号测试）：按 app_id 取脚本，
    /// pop front；只剩末值时复用。未预置 → 空观察（无信号）。
    pub pod_observations:
        DashMap<String, std::collections::VecDeque<Vec<container_runtime_api::PodObservation>>>,
    /// start_app（scale>0）后 phase 停在 Error：模拟新版本启动即崩（部署段
    /// 等待器的 Error 态快速失败测试用）。
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
    /// env 分派断言用（dev→UserappBuilder / prod→Userapp）。
    pub volume_name_calls: DashMap<String, Vec<String>>,
    /// list_workspace_identifiers 按 service_type 的返回预置 + 调用计数
    /// （storage get/query 的 env 分派断言用）。
    pub workspace_ids: DashMap<String, Vec<String>>,
    pub list_workspace_calls: AtomicUsize,
    /// destroy_app_pvc 调用计数（purge_app 断言用；trait 默认 no-op 故须覆写）
    pub destroy_pvc_calls: AtomicUsize,
    /// 注入 destroy_app_pvc 失败（true → ConnectionError）
    pub destroy_pvc_fails: AtomicBool,
    /// 注入 create_deployment 返回 CreationAborted（创建编排安全结束证明
    /// 测试）：Some((failed_at, definitive)) —— source 随 definitive 选
    /// Conflict/Timeout，retained 附样例清单。
    pub create_abort: std::sync::Mutex<Option<(container_runtime_api::CreationStage, bool)>>,
}

#[async_trait]
impl WorkspaceRuntime for MockRuntime {
    async fn ensure_workspace(
        &self,
        _identifier: &str,
        _service_type: &ServiceType,
        _storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        self.ensure_workspace_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn destroy_app_storage_snapshot(
        &self,
        snapshot: &shared_types::AppDeletionSnapshot,
    ) -> ContainerRuntimeResult<()> {
        self.destroy_app_pvc(&snapshot.app_id).await
    }
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

    async fn destroy_app_pvc(&self, _app_id: &str) -> ContainerRuntimeResult<()> {
        self.destroy_pvc_calls.fetch_add(1, Ordering::Relaxed);
        if self.destroy_pvc_fails.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ConnectionError(
                "mock destroy_app_pvc failure".into(),
            ));
        }
        Ok(())
    }

    async fn capture_app_storage_resize(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppStorageResizeTarget>> {
        Ok(Some(shared_types::UserAppStorageResizeTarget {
            context: context.clone(),
            resource: shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::PersistentVolumeClaim,
                name: format!("test-pvc-{}", context.app_id),
                uid: format!("pvc-{}", context.lifecycle_id),
                resource_version: Some("1".into()),
            },
            current_size: "100Gi".into(),
        }))
    }

    async fn resize_app_storage_target(
        &self,
        target: &shared_types::UserAppStorageResizeTarget,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome> {
        self.resize_app_storage(&target.context.app_id, new_size)
            .await
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

struct MockOperationLease(Arc<AtomicBool>, String, ServiceType);
#[async_trait]
impl shared_types::AppOperationLease for MockOperationLease {
    fn receipt(&self) -> Option<shared_types::UserAppOperationLeaseReceipt> {
        let family = self.2;
        let prefix = if family == ServiceType::UserappBuilder {
            "builder"
        } else {
            "prod"
        };
        Some(shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type: family,
            namespace: "test".into(),
            name: format!("rcoder-operation-{prefix}-{}", self.1),
            uid: format!("lease-{}", self.1),
            resource_version: "1".into(),
            token: "test-operation".into(),
        })
    }
    async fn release(self: Box<Self>) -> Result<(), String> {
        self.0.store(false, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl UserAppDeploymentRuntime for MockRuntime {
    async fn patch_app_policy_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        _policy: &shared_types::UserAppRuntimePolicy,
    ) -> ContainerRuntimeResult<()> {
        assert_eq!(
            target.resource.uid,
            format!("test-{}", target.context.lifecycle_id)
        );
        self.policy_calls.fetch_add(1, Ordering::SeqCst);
        // Docker policy is persisted by the coordinator, with no runtime writes.
        Ok(())
    }
    async fn update_env_configmap_if_version(
        &self,
        _app_id: &str,
        _env: &std::collections::HashMap<String, String>,
        _snapshot: &shared_types::AppEnvSnapshot,
    ) -> ContainerRuntimeResult<()> {
        match self.env_commit_failure.load(Ordering::SeqCst) {
            1 => Err(ContainerRuntimeError::Conflict(
                "env compare-and-swap rejected".into(),
            )),
            2 => Err(ContainerRuntimeError::ConnectionError(
                "env result uncertain".into(),
            )),
            _ => Ok(()),
        }
    }

    async fn acquire_builder_family_operation(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Box<dyn shared_types::AppOperationLease>> {
        if self
            .lease_held
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(ContainerRuntimeError::OperationInProgress(Box::new(
                shared_types::UserAppOperationInProgress {
                    app_id: app_id.into(),
                    service_type: ServiceType::UserappBuilder,
                    resource_name: format!("rcoder-operation-builder-{app_id}"),
                    operation_id: None,
                },
            )));
        }
        Ok(Box::new(MockOperationLease(
            self.lease_held.clone(),
            app_id.into(),
            ServiceType::UserappBuilder,
        )))
    }

    async fn acquire_app_operation(
        &self,
        _app_id: &str,
    ) -> ContainerRuntimeResult<Option<Box<dyn shared_types::AppOperationLease>>> {
        if self
            .lease_held
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // 对齐真实 K8s 运行时：409 → OperationInProgress（等待语义判据）
            return Err(ContainerRuntimeError::OperationInProgress(Box::new(
                shared_types::UserAppOperationInProgress {
                    app_id: _app_id.into(),
                    service_type: ServiceType::Userapp,
                    resource_name: format!("rcoder-operation-prod-{_app_id}"),
                    operation_id: None,
                },
            )));
        }
        Ok(Some(Box::new(MockOperationLease(
            self.lease_held.clone(),
            _app_id.into(),
            ServiceType::Userapp,
        ))))
    }

    async fn capture_app_deletion(
        &self,
        app_id: &str,
        expected: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::AppDeletionSnapshot> {
        let current = self.get_deployment_status(app_id).await?;
        if let Some(expected) = expected
            && current.as_ref().and_then(|s| s.resource_version.as_deref()) != Some(expected)
        {
            return Err(ContainerRuntimeError::Conflict(
                "deletion version changed".into(),
            ));
        }
        Ok(shared_types::AppDeletionSnapshot {
            app_id: app_id.into(),
            operation_id: "test-operation".into(),
            resources: vec![],
        })
    }

    async fn delete_app_snapshot(
        &self,
        snapshot: &shared_types::AppDeletionSnapshot,
    ) -> ContainerRuntimeResult<()> {
        self.delete_deployment(&snapshot.app_id).await
    }
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

    async fn observe_app_pods(
        &self,
        app_id: &str,
        _target: &container_runtime_api::AppDeployTarget,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::PodObservation>> {
        if let Some(mut script) = self.pod_observations.get_mut(app_id) {
            if script.len() > 1 {
                return Ok(script.pop_front().expect("scripted observations"));
            }
            return Ok(script.front().cloned().unwrap_or_default());
        }
        Ok(Vec::new())
    }

    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        let barrier = self.status_barrier.lock().expect("status barrier").clone();
        if let Some(barrier) = barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
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

    async fn capture_app_mutation_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_resource_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::UserAppMutationTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let status = self
            .get_deployment_status(&context.app_id)
            .await?
            .ok_or_else(|| ContainerRuntimeError::Conflict("Stop target missing".into()))?;
        if expected_resource_version
            .is_some_and(|value| status.resource_version.as_deref() != Some(value))
        {
            return Err(ContainerRuntimeError::Conflict(
                "Stop target version changed".into(),
            ));
        }
        Ok(shared_types::UserAppMutationTarget {
            context: context.clone(),
            resource: shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::Deployment,
                name: context.app_id.clone(),
                uid: format!("test-{}", context.lifecycle_id),
                resource_version: status.resource_version,
            },
        })
    }

    async fn restart_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_app_target(target).await
    }

    async fn start_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.scale_calls.fetch_add(1, Ordering::SeqCst);
        match self.deployments.entry(target.context.app_id.clone()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                if entry.get().resource_version != target.resource.resource_version {
                    return Err(ContainerRuntimeError::Conflict(
                        "Start target version changed".into(),
                    ));
                }
                let status = entry.get_mut();
                status.replicas = 1;
                status.ready_replicas = 1;
                status.phase = if self.crash_on_start.load(Ordering::SeqCst) {
                    "Error"
                } else {
                    "Running"
                }
                .into();
                status.wake_on_traffic = Some(true);
                Ok(())
            }
            dashmap::mapref::entry::Entry::Vacant(_) => Err(ContainerRuntimeError::Conflict(
                "Start target removed".into(),
            )),
        }
    }

    async fn stop_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        wake_on_traffic: bool,
    ) -> ContainerRuntimeResult<()> {
        self.scale_calls.fetch_add(1, Ordering::SeqCst);
        let failure = self.stop_failure_status.load(Ordering::SeqCst);
        if failure != 0 {
            if let Some(rejection) = shared_types::RuntimeRequestRejection::from_status(
                failure as u16,
                "Injected stop request rejection".into(),
            ) {
                return Err(ContainerRuntimeError::RequestRejected(rejection));
            }
            return Err(ContainerRuntimeError::ConnectionError(
                "Injected uncertain stop result".into(),
            ));
        }
        // One entry guard models the runtime's conditional atomic stop commit.
        match self.deployments.entry(target.context.app_id.clone()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                if entry.get().resource_version != target.resource.resource_version {
                    return Err(ContainerRuntimeError::Conflict(
                        "Stop target version changed".into(),
                    ));
                }
                let status = entry.get_mut();
                status.replicas = 0;
                status.ready_replicas = 0;
                status.phase = "Stopped".into();
                status.wake_on_traffic = Some(wake_on_traffic);
                Ok(())
            }
            dashmap::mapref::entry::Entry::Vacant(_) => Err(ContainerRuntimeError::Conflict(
                "Stop target removed".into(),
            )),
        }
    }

    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        self.scale_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(mut entry) = self.deployments.get_mut(app_id) {
            let status = entry.value_mut();
            status.replicas = replicas;
            status.ready_replicas = replicas.max(0);
            status.phase = if replicas == 0 {
                "Stopped"
            } else if self.crash_on_start.load(Ordering::SeqCst) {
                // 注入"启动即崩"：start_app 后 phase=Error，部署段等待器
                //（wait_deploy_stage）首个轮询即失败（无竞态地构造失败场景）。
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
        if let Some((stage, definitive)) = *self
            .create_abort
            .lock()
            .expect("create_abort injection lock")
        {
            let source = if definitive {
                ContainerRuntimeError::Conflict("mock claim rejected".into())
            } else {
                ContainerRuntimeError::Timeout("mock claim outcome pending".into())
            };
            return Err(ContainerRuntimeError::CreationAborted {
                progress: container_runtime_api::CreationProgress {
                    failed_at: stage,
                    definitive_rejection: definitive,
                    retained_idempotent_resources: vec![
                        "workspace pvc ensured: rcoder-app-abort-workspace".into(),
                        "storage-claim annotations may persist".into(),
                    ],
                },
                source: Box::new(source),
            });
        }
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
        if self.patch_preparation_fails.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::PreparationFailed(
                shared_types::AppPreparationFailure {
                    message: "image preparation failed".into(),
                },
            ));
        }
        // update_app 路径：与 create 同构（不注入失败；登记 deployments 供 get_app）
        self.create_deployment(params).await
    }
}

/// 直构造 AppService（绕过 `new` 的 HostPathResolver/K8s 前置校验副作用），
/// Docker 模式 + 指定 workspace_root（测试 tempdir）。
pub(crate) async fn test_service(workspace_root: &Path, runtime: Arc<MockRuntime>) -> AppService {
    tokio::fs::create_dir_all(workspace_root)
        .await
        .expect("test workspace");
    let store = rcoder_storage::userapp_lifecycle::SqliteUserAppStore::open(
        &workspace_root.join(format!("metadata-{}.sqlite3", uuid::Uuid::new_v4())),
    )
    .await
    .expect("SQLite metadata store");
    let config = AppManagerConfig {
        workspace_root: Some(workspace_root.to_string_lossy().into_owned()),
        operation_lock_root: workspace_root.to_string_lossy().into_owned(),
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
        metadata: crate::runtime::metadata::AppMetadataStore::new(Arc::new(store)),
        dev_cleanup: std::sync::RwLock::new(None),
        dev_locator: std::sync::RwLock::new(None),
        builder_recovery: std::sync::RwLock::new(None),
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
version = "0.14.1"
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

/// 可控假 [`shared_types::UserappDevCleanup`]：记录 cleanup 调用，可注入失败
/// （purge_app 的 dev 回收链路断言用；注入方式同 dev_locator——测试直写
/// `service.dev_cleanup` 的 RwLock）。
#[derive(Default)]
pub(crate) struct StubDevCleanup {
    pub calls: Arc<AtomicUsize>,
    pub fails: Arc<AtomicBool>,
}

#[async_trait]
impl shared_types::UserappDevCleanup for StubDevCleanup {
    async fn capture(
        &self,
        app_id: &str,
    ) -> Result<Box<dyn shared_types::UserappDevDeletion>, String> {
        Ok(Box::new(StubDevDeletion {
            app_id: app_id.into(),
            calls: self.calls.clone(),
            fails: self.fails.clone(),
        }))
    }
}
struct StubDevDeletion {
    app_id: String,
    calls: Arc<AtomicUsize>,
    fails: Arc<AtomicBool>,
}
#[async_trait]
impl shared_types::UserappDevDeletion for StubDevDeletion {
    fn receipt(&self) -> shared_types::UserappDevDeletionReceipt {
        dev_deletion_receipt(&self.app_id)
    }

    async fn cleanup(self: Box<Self>) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fails.load(Ordering::SeqCst) {
            Err("mock dev cleanup failure".into())
        } else {
            Ok(())
        }
    }
}

pub(crate) fn dev_deletion_receipt(app_id: &str) -> shared_types::UserappDevDeletionReceipt {
    shared_types::UserappDevDeletionReceipt {
        runtime: shared_types::BuilderDeletionSnapshot {
            resource_binding: None,
            app_id: app_id.into(),
            operation_id: "fixture-builder-deletion".into(),
            resources: vec![shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::Container,
                name: format!("rcoder-app-builder-{app_id}"),
                uid: "fixture-builder-uid".into(),
                resource_version: None,
            }],
            docker_bind_cleanup: false,
        },
        registry: Some(shared_types::BuilderRegistryIdentity {
            generation: "fixture-project-generation".into(),
            container_id: "fixture-builder-uid".into(),
        }),
    }
}

/// Storage-only fixture: all resource inventories were empty. Exercise the same
/// confirmed-stage protocol without running a container engine.
pub(crate) async fn complete_empty_deletion_fixture(mut operation: crate::service::OwnedOperation) {
    use shared_types::UserAppDeletionStage as Stage;
    let context = operation.execution_context();
    let app_id = context.app_id.clone();
    let mut checkpoint = shared_types::UserAppDeletionCheckpoint {
        schema_version: 1,
        stage: Stage::Captured,
        context,
        kind: shared_types::UserAppOperationKind::DeleteApplication,
        production: shared_types::AppDeletionSnapshot {
            app_id: app_id.clone(),
            operation_id: "empty-production".into(),
            resources: vec![],
        },
        development: Some(shared_types::UserappDevDeletionReceipt {
            runtime: shared_types::BuilderDeletionSnapshot {
                resource_binding: None,
                app_id,
                operation_id: "empty-development".into(),
                resources: vec![],
                docker_bind_cleanup: false,
            },
            registry: None,
        }),
    };
    operation
        .checkpoint(
            "resources_captured",
            serde_json::to_value(&checkpoint).expect("fixture checkpoint"),
        )
        .await
        .expect("captured");
    for stage in [
        Stage::ComputeRemoved,
        Stage::ProductionStorageRemoved,
        Stage::DevelopmentRemoved,
    ] {
        operation
            .deletion_progress(&mut checkpoint, stage)
            .await
            .expect("confirmed empty resource boundary");
    }
    operation
        .succeed()
        .await
        .expect("fixture deletion completion");
}
