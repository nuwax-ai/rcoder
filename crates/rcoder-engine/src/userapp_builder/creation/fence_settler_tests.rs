// ============================================================================
// P1 围栏证据化收束测试（泛化 per-kind 谓词）
// ============================================================================

use super::*;
use crate::app_state::AppState;
use arc_swap::ArcSwap;
use container_runtime_api::{
    AgentContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult, DeploymentStatus,
    UserAppDeploymentRuntime, WorkspaceRuntime,
};
use dashmap::DashMap;
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, ServiceType, UserAppAdmission,
    UserAppAdmissionOutcome, UserAppControlCommand, UserAppOperationKind,
};
use std::collections::HashMap;

/// validate_app_operation_receipt 的可控判定，模型各后端极性：
/// `Held` = Ok(true)（K8s 仍持有 / **Docker 孤儿 marker 的 authority 残留**）；
/// `NotHeld` = Ok(false)（K8s TTL 过期/对象缺失 / marker 已清）；
/// `TakenOver` = Err(Conflict)（K8s 被接管 / **Docker 活 flock "remains active"**）；
/// `Transport` = Err(K8sError)（查询/传输失败）。
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(crate) enum ReceiptVerdict {
    #[default]
    NotHeld,
    Held,
    TakenOver,
    Transport,
}

/// 终态落库种子：默认围栏形态（runtime_updated + 保持已有 checkpoint），
/// 或 D2 反例的完成证据形态（compute_confirmed + 显式 checkpoint）。
struct TerminalSeed {
    state: UserAppOperationState,
    step: &'static str,
    checkpoint: Option<serde_json::Value>,
}

impl TerminalSeed {
    fn fence(state: UserAppOperationState) -> Self {
        Self {
            state,
            step: "runtime_updated",
            checkpoint: None,
        }
    }
    fn completed_evidence(checkpoint: serde_json::Value) -> Self {
        Self {
            state: UserAppOperationState::RecoveryRequired,
            step: "compute_confirmed",
            checkpoint: Some(checkpoint),
        }
    }
}

/// 可控运行态：deployment 相位/副本 + 活代 generation + builder 观察 +
/// 物理租约 validate 判定 + 可选死亡探针覆写。
#[derive(Default)]
pub(crate) struct FenceRuntime {
    status: Mutex<Option<Option<DeploymentStatus>>>,
    generation: Mutex<Option<String>>,
    pub(crate) builder_workload: Mutex<Option<String>>,
    pub(crate) receipt_verdict: Mutex<ReceiptVerdict>,
    /// 显式 holder 死亡探针判定（模型 Docker flock 覆写）；
    /// None = 默认推导（K8s 极性）。
    pub(crate) receipt_holder_dead: Mutex<Option<bool>>,
}

impl FenceRuntime {
    pub(crate) fn scenario(
        status: Option<DeploymentStatus>,
        generation: Option<&str>,
    ) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Some(status)),
            generation: Mutex::new(generation.map(str::to_string)),
            builder_workload: Mutex::new(None),
            receipt_verdict: Mutex::new(ReceiptVerdict::NotHeld),
            receipt_holder_dead: Mutex::new(None),
        })
    }
}

fn status_of(phase: &str, replicas: i32) -> DeploymentStatus {
    DeploymentStatus {
        app_id: "fenced".into(),
        lifecycle_id: None,
        replicas,
        ready_replicas: replicas,
        phase: phase.into(),
        message: None,
        reason: None,
        pod_ip: None,
        node: None,
        restart_count: 0,
        started_at: None,
        ports: vec![],
        resource_version: Some("9".into()),
        recycle_enabled: None,
        idle_timeout_seconds: None,
        wake_on_traffic: None,
        created_at: None,
        deployment_uid: None,
    }
}

#[async_trait::async_trait]
impl AgentContainerRuntime for FenceRuntime {
    async fn create_container(
        &self,
        _params: container_runtime_api::ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Err(ContainerRuntimeError::ContainerNotFound("fence".into()))
    }
    async fn get_container_info(
        &self,
        _project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        _identifier: &str,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<container_runtime_api::RuntimeContainerInfo>> {
        Ok(None)
    }
    async fn stop_container(&self, _project_id: &str) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn is_container_running(&self, _project_id: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
    }
    async fn list_containers(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn inspect_builder_candidate(
        &self,
        _context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        // workload=None → capture_bound_target 回落到 capture_builder_control。
        Ok(BuilderControlTarget {
            resource_binding: None,
            context: _context.clone(),
            workload: None,
            pod: None,
            restart_image: None,
        })
    }
    async fn capture_builder_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        let workload = self.builder_workload.lock().expect("lock").clone();
        Ok(BuilderControlTarget {
            resource_binding: None,
            context: context.clone(),
            workload: workload.map(|name| AppResourceIdentity {
                kind: AppResourceKind::StatefulSet,
                name,
                uid: "builder-uid".into(),
                resource_version: Some("3".into()),
            }),
            pod: None,
            restart_image: None,
        })
    }
}

#[async_trait::async_trait]
impl WorkspaceRuntime for FenceRuntime {}

#[async_trait::async_trait]
impl UserAppDeploymentRuntime for FenceRuntime {
    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        Ok(vec![])
    }
    async fn get_deployment_status(
        &self,
        _app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        Ok(self.status.lock().expect("lock").clone().flatten())
    }
    async fn get_app_container_spec(
        &self,
        _app_id: &str,
    ) -> ContainerRuntimeResult<container_runtime_api::ContainerSpecSnapshot> {
        Ok(container_runtime_api::ContainerSpecSnapshot {
            command: None,
            env: Some(HashMap::from([(
                shared_types::APP_DEPLOY_GENERATION_ID.to_string(),
                self.generation
                    .lock()
                    .expect("lock")
                    .clone()
                    .unwrap_or_else(|| "absent".into()),
            )])),
            secrets: None,
            resources: None,
            health_check: None,
            ports: None,
        })
    }

    async fn validate_app_operation_receipt(
        &self,
        _context: &shared_types::UserAppExecutionContext,
        _receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        match *self.receipt_verdict.lock().expect("lock") {
            ReceiptVerdict::Held => Ok(true),
            ReceiptVerdict::NotHeld => Ok(false),
            ReceiptVerdict::TakenOver => {
                Err(ContainerRuntimeError::Conflict("mock taken over".into()))
            }
            ReceiptVerdict::Transport => Err(ContainerRuntimeError::K8sError("io".into())),
        }
    }

    async fn app_operation_receipt_holder_dead(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        if let Some(dead) = *self.receipt_holder_dead.lock().expect("lock") {
            return Ok(dead);
        }
        // None：复刻 trait 默认推导（模型 K8s 极性）。
        match self.validate_app_operation_receipt(context, receipt).await {
            Ok(held) => Ok(!held),
            Err(ContainerRuntimeError::Conflict(_)) => Ok(true),
            Err(error) => Err(error),
        }
    }
}

pub(crate) async fn fence_state(
    runtime: Arc<FenceRuntime>,
    kind: UserAppOperationKind,
    operation_id: &str,
) -> (Arc<AppState>, tempfile::TempDir) {
    fence_state_custom(runtime, kind, operation_id, None, None).await
}

/// 带围栏收束 grace 覆盖与可选租约绑定的围栏夹具（holder 死亡兜底反例用）。
pub(crate) async fn fence_state_custom(
    runtime: Arc<FenceRuntime>,
    kind: UserAppOperationKind,
    operation_id: &str,
    fence_settle_grace_secs: Option<u64>,
    bind_receipt: Option<shared_types::UserAppOperationLeaseReceipt>,
) -> (Arc<AppState>, tempfile::TempDir) {
    fence_state_with_terminal(
        runtime,
        kind,
        operation_id,
        UserAppOperationState::RecoveryRequired,
        fence_settle_grace_secs,
        bind_receipt,
    )
    .await
}

/// D2 反例夹具：StopBuilder 携带完成证据（compute_confirmed + 完成态
/// checkpoint）落 RecoveryRequired 围栏 + 可选租约绑定。
pub(crate) async fn fence_state_completed_builder(
    runtime: Arc<FenceRuntime>,
    operation_id: &str,
    bind_receipt: Option<shared_types::UserAppOperationLeaseReceipt>,
) -> (Arc<AppState>, tempfile::TempDir) {
    let checkpoint = serde_json::json!({
        "target": {"context": {"operation_id": operation_id}},
        "result": {"operation_id": operation_id},
    });
    fence_state_on_store_wrapped(
        runtime,
        UserAppOperationKind::StopBuilder,
        operation_id,
        None,
        bind_receipt,
        TerminalSeed::completed_evidence(checkpoint),
    )
    .await
}

async fn fence_state_with_terminal(
    runtime: Arc<FenceRuntime>,
    kind: UserAppOperationKind,
    operation_id: &str,
    terminal: UserAppOperationState,
    fence_settle_grace_secs: Option<u64>,
    bind_receipt: Option<shared_types::UserAppOperationLeaseReceipt>,
) -> (Arc<AppState>, tempfile::TempDir) {
    fence_state_on_store_wrapped(
        runtime,
        kind,
        operation_id,
        fence_settle_grace_secs,
        bind_receipt,
        TerminalSeed::fence(terminal),
    )
    .await
}

async fn fence_state_on_store_wrapped(
    runtime: Arc<FenceRuntime>,
    kind: UserAppOperationKind,
    operation_id: &str,
    fence_settle_grace_secs: Option<u64>,
    bind_receipt: Option<shared_types::UserAppOperationLeaseReceipt>,
    seed: TerminalSeed,
) -> (Arc<AppState>, tempfile::TempDir) {
    let metadata_dir = tempfile::tempdir().expect("metadata directory");
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &metadata_dir.path().join("userapp.turso.db"),
        )
        .await
        .expect("store"),
    );
    let (state, _keep) = fence_state_on_store(
        runtime,
        kind,
        operation_id,
        store,
        fence_settle_grace_secs,
        bind_receipt,
        seed,
    )
    .await;
    // TempDir 必须活过测试：把 _keep 换成泄露（测试进程级可接受）。
    std::mem::forget(metadata_dir);
    (state, tempfile::tempdir().expect("placeholder dir"))
}

/// PG 后端变体：store 走 Postgres（集群形态），RCODER_PG_TEST_DSN 门控。
#[cfg(feature = "rcoder-pg")]
pub(crate) async fn fence_state_pg(
    runtime: Arc<FenceRuntime>,
    kind: UserAppOperationKind,
    operation_id: &str,
    dsn: &str,
) -> Arc<AppState> {
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(
            &rcoder_storage::config::PostgresConfig {
                url: Some(dsn.into()),
                ..Default::default()
            },
        )
        .await
        .expect("PG store"),
    );
    let (state, _keep) = fence_state_on_store(
        runtime,
        kind,
        operation_id,
        store,
        None,
        None,
        TerminalSeed::fence(UserAppOperationState::RecoveryRequired),
    )
    .await;
    std::mem::forget(_keep);
    state
}

async fn fence_state_on_store(
    runtime: Arc<FenceRuntime>,
    kind: UserAppOperationKind,
    operation_id: &str,
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    fence_settle_grace_secs: Option<u64>,
    bind_receipt: Option<shared_types::UserAppOperationLeaseReceipt>,
    seed: TerminalSeed,
) -> (
    Arc<AppState>,
    Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
) {
    let (adapter, _cleanup_rx) =
        crate::storage::ProjectAdapter::new("test-ns".to_string(), "cluster.local".to_string());
    let activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
        300,
    )));
    let manager_config = app_manager::config::AppManagerConfig {
        access_mode: app_manager::config::AppAccessMode::Docker,
        ..app_manager::config::AppManagerConfig::default()
    };
    let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
        app_manager::service::AppService::new(
            manager_config,
            runtime.clone(),
            activity.clone(),
            None,
            store.clone(),
        )
        .await
        .expect("AppService"),
    );
    let download_dir = tempfile::tempdir().expect("download directory");
    let (pod_created_tx, _) = tokio::sync::broadcast::channel(8);
    let state = Arc::new(AppState {
        userapp_store: store.clone(),
        userapp_store_control: store.clone(),
        userapp_op_flight: Arc::new(
            crate::userapp_builder::shutdown_gate::OperationFlightGate::default(),
        ),
        userapp_recovery_handle: Arc::new(Mutex::new(None)),
        config: crate::config::AppConfig {
            fence_settle_grace_secs,
            ..Default::default()
        },
        projects: Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
            adapter,
        ))),
        pingora_service: None,
        userapp_error_page: None,
        grpc_pool: Arc::new(crate::grpc::GrpcChannelPool::new()),
        session_stream_registry: Arc::new(crate::grpc::SessionStreamRegistry::new()),
        api_key_config: Arc::new(ArcSwap::from_pointee(
            crate::config::ApiKeyAuthConfig::default(),
        )),
        pod_creating: Arc::new(DashMap::new()),
        pod_created_tx: Arc::new(pod_created_tx),
        container_prefix_rcoder: "dev-rcoder".to_string(),
        container_prefix_computer: "computer-agent-runner".to_string(),
        runtime,
        cleanup_rx: Arc::new(Mutex::new(None)),
        agent_download_manager: Arc::new(
            agent_provisioning::AgentDownloadManager::new(download_dir.path())
                .expect("download manager"),
        ),
        app_service,
        activity,
        cluster_domain: "cluster.local".to_string(),
    });
    // 构造围栏记录：admit(Pending) → advance(Running, executor) →
    // advance(RecoveryRequired, step=runtime_updated)。
    let lifecycle = store.ensure_identity("fenced").await.expect("identity");
    // 命令↔kind 严格对应（权威映射 = UserAppControlCommand::kind() 的
    // 穷尽 match）；EnsureBuilder/AdoptBuilder/AdoptApplication/HotDeploy
    // 无对应命令变体传 None。**禁止通配臂**：新增 kind 必须在此显式补
    // 映射，否则 admit 的命令↔摘要校验会拒绝（09-22 用户纪律：穷尽
    // match，`_` 会静默吞掉新增变体）。仅携带 input_digest 的命令附带
    // 执行输入。
    let input = shared_types::UserAppExecutionInput::new("{}".into());
    let digest = input.digest();
    let (command, with_input) = match kind {
        UserAppOperationKind::EnsureBuilder
        | UserAppOperationKind::AdoptBuilder
        | UserAppOperationKind::AdoptApplication
        | UserAppOperationKind::HotDeploy => (None, false),
        UserAppOperationKind::StartDeployment => (
            Some(UserAppControlCommand::Deploy {
                restart: false,
                input_digest: digest,
            }),
            true,
        ),
        UserAppOperationKind::RestartDeployment => (
            Some(UserAppControlCommand::Deploy {
                restart: true,
                input_digest: digest,
            }),
            true,
        ),
        UserAppOperationKind::Create => (
            Some(UserAppControlCommand::Create {
                input_digest: digest,
            }),
            true,
        ),
        UserAppOperationKind::Update => (
            Some(UserAppControlCommand::Update {
                input_digest: digest,
            }),
            true,
        ),
        UserAppOperationKind::Start => {
            (Some(UserAppControlCommand::Start { traffic: false }), false)
        }
        UserAppOperationKind::Restart => (Some(UserAppControlCommand::Restart), false),
        UserAppOperationKind::Stop => (
            Some(UserAppControlCommand::Stop {
                wake_on_traffic: true,
            }),
            false,
        ),
        UserAppOperationKind::SetRecyclePolicy => (
            Some(UserAppControlCommand::SetRecyclePolicy {
                policy: shared_types::UserAppRuntimePolicy {
                    recycle_enabled: Some(true),
                    idle_timeout_seconds: Some(7200),
                    wake_on_traffic: Some(true),
                },
            }),
            false,
        ),
        UserAppOperationKind::DeleteCompute => (
            Some(UserAppControlCommand::DeleteResources {
                purge: false,
                expected_resource_version: None,
            }),
            false,
        ),
        UserAppOperationKind::PurgeResources => (
            Some(UserAppControlCommand::DeleteResources {
                purge: true,
                expected_resource_version: None,
            }),
            false,
        ),
        UserAppOperationKind::DeleteApplication => {
            (Some(UserAppControlCommand::DeleteApplication), false)
        }
        UserAppOperationKind::DestroyDevStorage => (
            Some(UserAppControlCommand::DestroyStorage { production: false }),
            false,
        ),
        UserAppOperationKind::DestroyProdStorage => (
            Some(UserAppControlCommand::DestroyStorage { production: true }),
            false,
        ),
        UserAppOperationKind::ClearDevStorage => (
            Some(UserAppControlCommand::ClearStorage { production: false }),
            false,
        ),
        UserAppOperationKind::ClearProdStorage => (
            Some(UserAppControlCommand::ClearStorage { production: true }),
            false,
        ),
        UserAppOperationKind::ResetDevDatabasePassword => (
            Some(UserAppControlCommand::ResetDatabasePassword {
                production: false,
                username: "app".into(),
            }),
            false,
        ),
        UserAppOperationKind::ResetProdDatabasePassword => (
            Some(UserAppControlCommand::ResetDatabasePassword {
                production: true,
                username: "app".into(),
            }),
            false,
        ),
        UserAppOperationKind::PrepareProdDatabase => {
            (Some(UserAppControlCommand::PrepareProdDatabase), false)
        }
        UserAppOperationKind::StopBuilder => (Some(UserAppControlCommand::StopBuilder), false),
        UserAppOperationKind::RestartBuilder => {
            (Some(UserAppControlCommand::RestartBuilder), false)
        }
    };
    let admitted = store
        .admit_with_input(
            &UserAppAdmission {
                app_id: "fenced".into(),
                lifecycle_id: Some(lifecycle.lifecycle_id.clone()),
                operation_id: operation_id.into(),
                request_id: Some(format!("req-{operation_id}")),
                request_fingerprint: "cd".repeat(32),
                kind,
                command,
                metadata: None,
                runtime_policy_on_success: None,
            },
            with_input.then_some(&input),
        )
        .await
        .expect("admit");
    let record = match admitted {
        UserAppAdmissionOutcome::Accepted(record) => record,
        other => panic!("unexpected admission: {other:?}"),
    };
    store
        .advance(&UserAppOperationProgress {
            app_id: record.app_id.clone(),
            lifecycle_id: record.lifecycle_id.clone(),
            operation_id: record.operation_id.clone(),
            expected_revision: record.revision,
            executor_id: "executor-fenced".into(),
            state: UserAppOperationState::Running,
            step: "claimed".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        })
        .await
        .expect("claim");
    if let Some(receipt) = &bind_receipt {
        store
            .bind_operation_lease(
                &shared_types::UserAppExecutionContext {
                    app_id: "fenced".into(),
                    lifecycle_id: lifecycle.lifecycle_id.clone(),
                    operation_id: operation_id.into(),
                    executor_id: "executor-fenced".into(),
                    request_fingerprint: "cd".repeat(32),
                },
                receipt,
            )
            .await
            .expect("bind lease");
    }
    let running = store
        .get_operation("fenced", operation_id)
        .await
        .expect("read")
        .expect("running record");
    let checkpoint = seed
        .checkpoint
        .unwrap_or_else(|| running.checkpoint.clone());
    seed_terminal_fence(&store, &running, seed.state, seed.step, checkpoint).await;
    (state, store)
}

/// 终态/围栏转移：失败与围栏保持已记录 checkpoint（domain 对删除/存储族
/// 有「必须保持证据」校验，真实执行器 fail 亦保点）；D2 反例用
/// compute_confirmed + 完成证据 checkpoint 复用同一落库路径。
async fn seed_terminal_fence(
    store: &Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    running: &UserAppOperationRecord,
    terminal: UserAppOperationState,
    step: &str,
    checkpoint: serde_json::Value,
) {
    store
        .advance(&UserAppOperationProgress {
            app_id: running.app_id.clone(),
            lifecycle_id: running.lifecycle_id.clone(),
            operation_id: running.operation_id.clone(),
            expected_revision: running.revision,
            executor_id: "executor-fenced".into(),
            state: terminal,
            step: step.into(),
            checkpoint,
            error_code: Some("ERR_BACKEND_ERROR".into()),
            error_message: Some(
                "Capture explicit database target: Owned management container is not running"
                    .into(),
            ),
        })
        .await
        .expect("fence");
}

pub(crate) async fn settled(state: &Arc<AppState>, operation_id: &str) -> UserAppOperationRecord {
    state
        .userapp_store
        .get_operation("fenced", operation_id)
        .await
        .expect("read")
        .expect("record")
}

/// app 154 事故类：发布 rollout 已完成（活代=本操作、相位 Running）→
/// 围栏必须被证据化收束为 Failed（修复前永久卡死，需人工 SQL）。
#[tokio::test]
async fn start_deployment_fence_settles_when_rollout_completed() {
    let operation_id = "op-rollout-done";
    let runtime = FenceRuntime::scenario(Some(status_of("Running", 1)), Some(operation_id));
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::Failed);
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["kind"],
        "rollout_completed"
    );
    assert!(
        record
            .error_message
            .as_deref()
            .is_some_and(|m| m.contains("fence released")),
        "{:?}",
        record.error_message
    );
}

/// 被后续发布取代（活代属别的操作）同样是确定性状态——与 builder 族
/// "被另一次操作 ensure" 同义，收束并留痕。
#[tokio::test]
async fn start_deployment_fence_settles_when_superseded_by_newer_generation() {
    let operation_id = "op-superseded";
    let runtime = FenceRuntime::scenario(Some(status_of("Running", 1)), Some("op-newer"));
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::Failed);
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["kind"],
        "superseded_by_newer_generation"
    );
}

/// 运行态查不到（部署不存在）：证据不足，围栏保持。
#[tokio::test]
async fn start_deployment_fence_kept_when_deployment_absent() {
    let operation_id = "op-absent";
    let runtime = FenceRuntime::scenario(None, Some(operation_id));
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("scan");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
    assert!(
        record
            .checkpoint
            .get("superseded_by_live_builder")
            .is_none()
    );
}

/// 无证据谓词的 kind：即使运行态可见也保持围栏——保守面不因泛化而
/// 扩大（超龄+holder 死亡由兜底收束，见 holder_expired 系列）。
/// 注：本测试原以 Update 为靶，Step B 给 Create/Update 补谓词后按批准
/// 需求换到仍无谓词的存储族 kind（测试预期随需求偏移，语义保持）。
#[tokio::test]
async fn unsupported_kind_fence_kept_even_when_running() {
    let operation_id = "op-destroy-storage";
    let runtime = FenceRuntime::scenario(Some(status_of("Running", 1)), Some(operation_id));
    let (state, _dir) = fence_state(
        runtime,
        UserAppOperationKind::DestroyDevStorage,
        operation_id,
    )
    .await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("scan");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
}

/// Step B 反例（修复前必挂）：删除中断后部署已缺席 = 达成证据
/// （同 Stop 哲学），必须收束留痕而不是永久围栏。
#[tokio::test]
async fn delete_compute_fence_settles_when_deployment_absent() {
    let operation_id = "op-delete-absent";
    let runtime = FenceRuntime::scenario(None, None);
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::DeleteCompute, operation_id).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::Failed);
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["kind"], "absent_confirmed",
        "{:?}",
        record.checkpoint["fence_released_evidence"]
    );
}

/// Step B 反例（修复前必挂）：Create 报错且什么都没建出来 → no_trace
/// 收束（app-166 同族的 prod 面）。
#[tokio::test]
async fn create_fence_settles_no_trace_when_nothing_created() {
    let operation_id = "op-create-no-trace";
    let runtime = FenceRuntime::scenario(None, None);
    let (state, _dir) = fence_state(runtime, UserAppOperationKind::Create, operation_id).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::Failed);
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["kind"], "no_trace",
        "{:?}",
        record.checkpoint["fence_released_evidence"]
    );
}

/// Step B 反例（修复前必挂）：SetRecyclePolicy 的注解可读即确定状态
/// ——已达 desired 收束留痕（policy 观察值入证据）。
#[tokio::test]
async fn set_recycle_policy_fence_settles_with_observed_policy() {
    let operation_id = "op-recycle-policy";
    let mut status = status_of("Running", 1);
    status.recycle_enabled = Some(true);
    status.idle_timeout_seconds = Some(7200);
    status.wake_on_traffic = Some(true);
    let runtime = FenceRuntime::scenario(Some(status), None);
    let (state, _dir) = fence_state(
        runtime,
        UserAppOperationKind::SetRecyclePolicy,
        operation_id,
    )
    .await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::Failed);
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["kind"], "already_at_desired",
        "{:?}",
        record.checkpoint["fence_released_evidence"]
    );
}

/// builder 族原语义不回退：存活 builder（指纹无关观察）→ 收束。
#[tokio::test]
async fn ensure_builder_fence_settles_with_live_builder() {
    let operation_id = "op-builder-live";
    let runtime = FenceRuntime::scenario(None, None);
    *runtime.builder_workload.lock().expect("lock") = Some("rcoder-app-builder-fenced".into());
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::EnsureBuilder, operation_id).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::Failed);
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["kind"],
        "live_builder"
    );
}

/// builder 缺失：保持围栏（受保护目标未证实）。
#[tokio::test]
async fn ensure_builder_fence_kept_when_builder_absent() {
    let operation_id = "op-builder-absent";
    let runtime = FenceRuntime::scenario(None, None);
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::EnsureBuilder, operation_id).await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("scan");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
}

/// 判定表锁（探针化后，模型 K8s 极性的默认推导）：非持有（Ok(false)）
/// 与被接管（Err(Conflict)）都是 holder 死亡证明；传输失败保守保持围栏。
#[tokio::test]
async fn not_held_and_taken_over_settle_via_default_derivation() {
    for (verdict, operation_id) in [
        (ReceiptVerdict::NotHeld, "op-expired-lease"),
        (ReceiptVerdict::TakenOver, "op-taken-over-lease"),
    ] {
        let runtime = FenceRuntime::scenario(None, None);
        *runtime.receipt_verdict.lock().expect("lock") = verdict;
        let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type: ServiceType::UserappBuilder,
            namespace: "test-ns".into(),
            name: format!("rcoder-operation-builder-{operation_id}"),
            uid: "lease-uid".into(),
            resource_version: "1".into(),
            token: "lease-token".into(),
        };
        let (state, _dir) = fence_state_custom(
            runtime,
            UserAppOperationKind::EnsureBuilder,
            operation_id,
            Some(0),
            Some(receipt),
        )
        .await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("settle");
        let record = settled(&state, operation_id).await;
        assert_eq!(
            record.state,
            UserAppOperationState::Failed,
            "默认推导必须把 {verdict:?} 判为 holder 已死"
        );
        assert_eq!(
            record.checkpoint["fence_released_evidence"]["kind"],
            "holder_expired"
        );
    }
}

/// 探针传输失败（fail-safe）：保守保持围栏，绝不凭查询失败判死。
#[tokio::test]
async fn holder_death_probe_failure_keeps_fence() {
    let operation_id = "op-probe-transport";
    let runtime = FenceRuntime::scenario(None, None);
    *runtime.receipt_verdict.lock().expect("lock") = ReceiptVerdict::Transport;
    let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "test-ns".into(),
        name: format!("rcoder-operation-builder-{operation_id}"),
        uid: "lease-uid".into(),
        resource_version: "1".into(),
        token: "lease-token".into(),
    };
    let (state, _dir) = fence_state_custom(
        runtime,
        UserAppOperationKind::EnsureBuilder,
        operation_id,
        Some(0),
        Some(receipt),
    )
    .await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("scan");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
}

/// D1 反例之一（修复前必挂）：Docker flock 极性——validate 对孤儿
/// marker 返回 Ok(true)（authority 残留），旧推导把它判成「仍持有」
/// → 兜底死代码、Compose 上围栏永久保持。修复后走死亡探针
/// （flock 已被内核释放=已死），超龄围栏必须收束。
#[tokio::test]
async fn docker_orphan_marker_binding_settles_via_death_probe() {
    let operation_id = "op-docker-orphan";
    let runtime = FenceRuntime::scenario(None, None);
    *runtime.receipt_verdict.lock().expect("lock") = ReceiptVerdict::Held;
    *runtime.receipt_holder_dead.lock().expect("lock") = Some(true);
    let receipt = shared_types::UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 1,
        inode: 2,
        token: "orphan-token".into(),
    };
    let (state, _dir) = fence_state_custom(
        runtime,
        UserAppOperationKind::EnsureBuilder,
        operation_id,
        Some(0),
        Some(receipt),
    )
    .await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(
        record.state,
        UserAppOperationState::Failed,
        "孤儿 marker 不是活性证明，探针判死必须收束"
    );
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["kind"],
        "holder_expired"
    );
}

/// D1 反例之二（修复前必挂，危险面）：Docker 活 flock——validate 返回
/// Err(Conflict)（"remains active"），旧推导把 Conflict 一律当死亡证明
/// → 会从**活持有者**手里收束围栏。修复后探针（flock 被持有=未死）
/// 必须保持围栏。
#[tokio::test]
async fn docker_live_flock_binding_keeps_fence_under_probe() {
    let operation_id = "op-docker-live-flock";
    let runtime = FenceRuntime::scenario(None, None);
    *runtime.receipt_verdict.lock().expect("lock") = ReceiptVerdict::TakenOver;
    *runtime.receipt_holder_dead.lock().expect("lock") = Some(false);
    let receipt = shared_types::UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 1,
        inode: 2,
        token: "live-token".into(),
    };
    let (state, _dir) = fence_state_custom(
        runtime,
        UserAppOperationKind::EnsureBuilder,
        operation_id,
        Some(0),
        Some(receipt),
    )
    .await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("scan");
    let record = settled(&state, operation_id).await;
    assert_eq!(
        record.state,
        UserAppOperationState::RecoveryRequired,
        "活 flock 持有者未死，围栏必须保持"
    );
}

/// Step A 反例（修复前必挂，app-166 事故类）：创建报错落围栏后什么都没
/// 建出来（无 workload、无租约绑定）——持有者已死（0 绑定）且超龄
/// （grace=0）时必须兜底收束为 Failed，不再永久占受理 slot。
#[tokio::test]
async fn holder_expired_fence_without_binding_settles() {
    let operation_id = "op-holder-expired";
    let runtime = FenceRuntime::scenario(None, None); // 无 workload → 谓词 Insufficient
    let (state, _dir) = fence_state_custom(
        runtime,
        UserAppOperationKind::EnsureBuilder,
        operation_id,
        Some(0),
        None,
    )
    .await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("settle");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::Failed);
    assert_eq!(
        record.checkpoint["fence_released_evidence"]["kind"], "holder_expired",
        "evidence 必须留痕 holder 死亡判定: {:?}",
        record.checkpoint["fence_released_evidence"]
    );
    assert!(
        record
            .error_message
            .as_deref()
            .is_some_and(|m| m.contains("holder expired")),
        "{:?}",
        record.error_message
    );
    // 端到端：slot 已释放——同 app 同 scope 新受理必须 Accepted。
    let admitted = state
        .userapp_store
        .admit_with_input(
            &UserAppAdmission {
                app_id: "fenced".into(),
                lifecycle_id: Some(record.lifecycle_id.clone()),
                operation_id: format!("{operation_id}-retry"),
                request_id: Some(format!("req-{operation_id}-retry")),
                request_fingerprint: "ef".repeat(32),
                kind: UserAppOperationKind::EnsureBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            },
            None,
        )
        .await
        .expect("admit after settle");
    assert!(
        matches!(admitted, UserAppAdmissionOutcome::Accepted(_)),
        "收束后同 scope 必须可受理: {admitted:?}"
    );
}

/// D2 反例（修复前必挂）：StopBuilder 已携带完成证据，收尾清理的
/// release 失败（本 mock 的 release 走 trait 默认=Err；现实中对应租约
/// 被接管被包成 K8sError / 瞬态传输错误）。修复前 release 错误把已
/// 完成的操作翻回围栏（advance RecoveryRequired + 错误传播）——
/// completed→fence 翻转正是 154 类永久阻塞的成因；修复后清理失败只留
/// warn、绑定行交终态租约清扫重试，操作必须落 Succeeded。
#[tokio::test]
async fn completed_builder_survives_release_failure() {
    let operation_id = "op-release-fails";
    let runtime = FenceRuntime::scenario(None, None);
    let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "test-ns".into(),
        name: format!("rcoder-operation-builder-{operation_id}"),
        uid: "lease-uid".into(),
        resource_version: "1".into(),
        token: "lease-token".into(),
    };
    let (state, _dir) = fence_state_completed_builder(runtime, operation_id, Some(receipt)).await;
    let snapshot = settled(&state, operation_id).await;
    assert!(
        shared_types::userapp_operation_has_final_evidence(&snapshot),
        "fixture must carry final evidence"
    );
    super::super::control::reconcile_completed(&state, &snapshot)
        .await
        .expect("finalize");
    let record = settled(&state, operation_id).await;
    assert_eq!(
        record.state,
        UserAppOperationState::Succeeded,
        "release 清理失败不得把已完成操作翻回围栏"
    );
    // 清理记录（租约绑定行）保留，交终态租约清扫重试成功后 forget。
    assert!(
        state
            .userapp_store
            .get_operation_lease("fenced", operation_id)
            .await
            .expect("read binding")
            .is_some(),
        "release 失败时保留绑定行供终态清扫重试"
    );
}

/// 锁：围栏未超龄（默认 grace）→ 即使无绑定也保持围栏——兜底只对
/// 超龄围栏生效，给在途收尾留出宽限。
#[tokio::test]
async fn fresh_fence_without_binding_keeps() {
    let operation_id = "op-fresh-fence";
    let runtime = FenceRuntime::scenario(None, None);
    let (state, _dir) = fence_state_custom(
        runtime,
        UserAppOperationKind::EnsureBuilder,
        operation_id,
        None,
        None,
    )
    .await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("scan");
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
}

/// 锁：绑定行存在且物理租约仍持有（validate=true）→ holder 未死，
/// 即使超龄也保持围栏——兜底不得从活持有者手里收束。
#[tokio::test]
async fn live_lease_binding_keeps_fence() {
    let operation_id = "op-live-lease";
    let runtime = FenceRuntime::scenario(None, None);
    *runtime.receipt_verdict.lock().expect("lock") = ReceiptVerdict::Held;
    let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "test-ns".into(),
        name: format!("rcoder-operation-builder-{operation_id}"),
        uid: "lease-uid".into(),
        resource_version: "1".into(),
        token: "lease-token".into(),
    };
    let (state, _dir) = fence_state_custom(
        runtime,
        UserAppOperationKind::EnsureBuilder,
        operation_id,
        Some(0),
        Some(receipt),
    )
    .await;
    let snapshot = settled(&state, operation_id).await;
    reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect("scan");
    let record = settled(&state, operation_id).await;
    assert_eq!(
        record.state,
        UserAppOperationState::RecoveryRequired,
        "活租约必须保持围栏"
    );
}

/// app 159 事故回归：创建中的瞬时 RecoveryRequired 必须被骑代——3s 后
/// 恢复核验把操作收束为真终态 Failed，等待方观察到的是真终态而非在
/// 中间态上提前宣判。修复前必红：立即返回 "ended as RecoveryRequired"。
#[tokio::test]
async fn transient_fence_is_ridden_through_to_real_terminal() {
    let operation_id = "op-ride-through";
    let runtime = FenceRuntime::scenario(None, None);
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
    let fenced = settled(&state, operation_id).await;
    // 3s 后恢复扫描器的核验收束路径把围栏落为真终态（Failed+证据）。
    let store = state.userapp_store.clone();
    let snapshot = fenced.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        store
            .settle_fenced_operation(
                &snapshot,
                &serde_json::json!({"kind": "test"}),
                "test fixture settle",
            )
            .await
            .expect("settle");
    });
    let result = wait_record(
        &state.userapp_store,
        &fenced,
        Instant::now() + Duration::from_secs(30),
    )
    .await;
    let error = result.expect_err("terminal failure").to_string();
    assert!(
        error.contains("ended as Failed"),
        "waiter must observe the real terminal, not the transient fence: {error}"
    );
    assert!(
        !error.contains("RecoveryRequired"),
        "waiter must not report the intermediate state as terminal: {error}"
    );
}

/// 真围栏（持续不改判）在宽限后仍按终态失败——骑代不变成永久等待。
#[tokio::test]
async fn persistent_fence_fails_only_after_grace() {
    let operation_id = "op-persistent-fence";
    let runtime = FenceRuntime::scenario(None, None);
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
    let fenced = settled(&state, operation_id).await;
    let started = Instant::now();
    let result = wait_record(
        &state.userapp_store,
        &fenced,
        Instant::now() + Duration::from_secs(45),
    )
    .await;
    let error = result.expect_err("persistent fence must fail").to_string();
    assert!(error.contains("ended as RecoveryRequired"), "{error}");
    assert!(
        started.elapsed() >= RECOVERY_REQUIRED_GRACE,
        "must not fail before the grace window: {:?}",
        started.elapsed()
    );
}

/// 已是真终态（Failed）的操作保持立即失败——骑代只对中间态生效。
#[tokio::test]
async fn real_terminal_failure_still_fails_fast() {
    let operation_id = "op-already-failed";
    let runtime = FenceRuntime::scenario(None, None);
    let (state, _dir) = fence_state_with_terminal(
        runtime,
        UserAppOperationKind::StartDeployment,
        operation_id,
        UserAppOperationState::Failed,
        None,
        None,
    )
    .await;
    let failed = settled(&state, operation_id).await;
    let started = Instant::now();
    let result = wait_record(
        &state.userapp_store,
        &failed,
        Instant::now() + Duration::from_secs(10),
    )
    .await;
    let error = result.expect_err("failed operation must error").to_string();
    assert!(error.contains("ended as Failed"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "real terminal must fail fast: {:?}",
        started.elapsed()
    );
}
