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
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, ContainerBasicInfo, ServiceType,
    UserAppAdmission, UserAppAdmissionOutcome, UserAppControlCommand, UserAppOperationKind,
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
    error_message: Option<&'static str>,
}

impl TerminalSeed {
    fn fence(state: UserAppOperationState) -> Self {
        Self {
            state,
            step: "runtime_updated",
            checkpoint: None,
            error_message: None,
        }
    }
    fn completed_evidence(checkpoint: serde_json::Value) -> Self {
        Self {
            state: UserAppOperationState::RecoveryRequired,
            step: "compute_confirmed",
            checkpoint: Some(checkpoint),
            error_message: None,
        }
    }
}

/// 可控运行态：deployment 相位/副本 + 活代 generation + builder 观察 +
/// 物理租约 validate 判定 + 可选死亡探针覆写。
#[derive(Default)]
pub(crate) struct FenceRuntime {
    pub(crate) builder_create_calls: std::sync::atomic::AtomicUsize,
    pub(crate) retained_creation_absent: std::sync::atomic::AtomicBool,
    pub(crate) retained_storage: Mutex<Option<shared_types::UserAppRetainedStorage>>,
    pub(crate) agent_replacement: Mutex<Option<shared_types::VerifiedAgentReplacement>>,
    pub(crate) agent_replacement_verifications: std::sync::atomic::AtomicUsize,
    pub(crate) agent_replacement_transport_fails: std::sync::atomic::AtomicBool,
    pub(crate) previous_compute_releases: std::sync::atomic::AtomicUsize,
    pub(crate) previous_compute_arguments: Mutex<
        Vec<(
            shared_types::UserAppExecutionContext,
            shared_types::UserAppExecutionContext,
            shared_types::UserAppOperationLeaseReceipt,
        )>,
    >,
    pub(crate) compute_release_succeeds: std::sync::atomic::AtomicBool,
    pub(crate) compute_stall: std::sync::atomic::AtomicBool,
    pub(crate) compute_inspection: Mutex<Option<shared_types::ComputeLeaseInspection>>,
    /// Optional actual OS lease fixture; other runtime behavior stays unchanged.
    pub(crate) physical_compute_runtime: Mutex<Option<Arc<dyn UserAppDeploymentRuntime>>>,
    pub(crate) compute_prepares: std::sync::atomic::AtomicUsize,
    pub(crate) compute_releases: std::sync::atomic::AtomicUsize,
    pub(crate) hot_observation: Mutex<Option<container_runtime_api::AppOperationLeaseObservation>>,
    pub(crate) hot_observation_replies: Mutex<
        std::collections::VecDeque<
            ContainerRuntimeResult<container_runtime_api::AppOperationLeaseObservation>,
        >,
    >,
    pub(crate) hot_target: Mutex<Option<shared_types::RuntimeConfigurationTarget>>,
    pub(crate) hot_owner: Mutex<Option<serde_json::Value>>,
    pub(crate) hot_env: Mutex<Option<shared_types::AppEnvSnapshot>>,
    pub(crate) hot_reads: std::sync::atomic::AtomicUsize,
    pub(crate) hot_writes: std::sync::atomic::AtomicUsize,
    status: Mutex<Option<Option<DeploymentStatus>>>,
    generation: Mutex<Option<String>>,
    pub(crate) builder_workload: Mutex<Option<String>>,
    pub(crate) receipt_verdict: Mutex<ReceiptVerdict>,
    /// app_compute_absent 探测开关，正向证实 image-roll 目标已不存在。
    pub(crate) app_target_absent: std::sync::atomic::AtomicBool,
    /// Only an explicit backend absence witness authorizes Dev convergence.
    /// The default false never turns ordinary discovery None into proof.
    pub(crate) builder_absent_proven: std::sync::atomic::AtomicBool,
    pub(crate) builder_absence_error: Mutex<Option<String>>,
    pub(crate) builder_absence_probes: std::sync::atomic::AtomicUsize,
    pub(crate) app_absence_error: Mutex<Option<String>>,
    pub(crate) app_absence_probes: std::sync::atomic::AtomicUsize,
    /// 显式 holder 死亡探针判定（模型 Docker flock 覆写）；
    /// None = 默认推导（K8s 极性）。
    pub(crate) receipt_holder_dead: Mutex<Option<bool>>,
    /// 结算对账证据旋钮：true 时 reconcile_builder_compute_start 报告运行中。
    pub(crate) builder_start_observed: std::sync::atomic::AtomicBool,
    /// 结算对账证据旋钮：true 时 prepare_builder_compute_retry 报告
    /// Ok(None)（启动已提交、只能观察——契约原文语义）。
    pub(crate) builder_retry_committed: std::sync::atomic::AtomicBool,
    /// 结算对账证据旋钮：true 时 prepare_builder_compute_retry 报告
    /// Ok(Some)（存在可重试目标——契约原文语义）；优先于 committed。
    pub(crate) builder_retry_partial: std::sync::atomic::AtomicBool,
    /// 结算对账证据旋钮：true 时 observe_builder_compute_running 报告
    /// Ok(true)（捕获控制器声明副本≥1，无需任何操作收据）。
    pub(crate) builder_running_observed: std::sync::atomic::AtomicBool,
    /// apply_builder_control 调用记录（workload uid, name, restart 标志）：
    /// 交接测试断言后继对正确 UID 的物理控制动作。
    pub(crate) builder_control_calls: Mutex<Vec<(String, String, bool)>>,
    /// 免回执的原创建运行时权威观测（陈旧 Running 对账用）；
    /// None = trait 默认 Unsupported（保守保持）。
    pub(crate) unbound_creation_authority:
        Mutex<Option<container_runtime_api::UnboundBuilderCreationAuthority>>,
    /// 注入免回执权威观测传输失败（fail-safe 反例）。
    pub(crate) unbound_authority_transport_fails: std::sync::atomic::AtomicBool,
}

impl FenceRuntime {
    pub(crate) fn scenario(
        status: Option<DeploymentStatus>,
        generation: Option<&str>,
    ) -> Arc<Self> {
        Arc::new(Self {
            builder_create_calls: Default::default(),
            retained_creation_absent: Default::default(),
            retained_storage: Default::default(),
            agent_replacement: Default::default(),
            agent_replacement_verifications: Default::default(),
            agent_replacement_transport_fails: Default::default(),
            previous_compute_releases: Default::default(),
            previous_compute_arguments: Mutex::new(Vec::new()),
            compute_release_succeeds: Default::default(),
            compute_stall: Default::default(),
            compute_inspection: Default::default(),
            physical_compute_runtime: Default::default(),
            compute_prepares: Default::default(),
            compute_releases: Default::default(),
            hot_observation: Default::default(),
            hot_observation_replies: Default::default(),
            hot_target: Default::default(),
            hot_owner: Default::default(),
            hot_env: Default::default(),
            hot_reads: Default::default(),
            hot_writes: Default::default(),
            status: Mutex::new(Some(status)),
            generation: Mutex::new(generation.map(str::to_string)),
            builder_workload: Mutex::new(None),
            receipt_verdict: Mutex::new(ReceiptVerdict::NotHeld),
            app_target_absent: Default::default(),
            builder_absent_proven: Default::default(),
            builder_absence_error: Default::default(),
            builder_absence_probes: Default::default(),
            app_absence_error: Default::default(),
            app_absence_probes: Default::default(),
            receipt_holder_dead: Mutex::new(None),
            builder_start_observed: Default::default(),
            builder_retry_committed: Default::default(),
            builder_retry_partial: Default::default(),
            builder_running_observed: Default::default(),
            builder_control_calls: Mutex::new(Vec::new()),
            unbound_creation_authority: Mutex::new(None),
            unbound_authority_transport_fails: Default::default(),
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
    // -- settlement-evidence knobs (deadline takeover tests) ------------------

    async fn reconcile_builder_compute_start(
        &self,
        _target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        if self
            .builder_start_observed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(Some(ContainerBasicInfo {
                container_id: "settled-fixture-container".into(),
                container_name: "settled-fixture".into(),
                container_ip: "127.0.0.1".into(),
                internal_port: 1,
                external_port: 1,
                project_id: _target.context.app_id.clone(),
                status: "running".into(),
                created_at: chrono::Utc::now(),
                service_url: String::new(),
                workload_uid: _target.workload.as_ref().map(|w| w.uid.clone()),
            }));
        }
        Ok(None)
    }

    async fn prepare_builder_compute_retry(
        &self,
        _target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<BuilderControlTarget>> {
        if self
            .builder_retry_partial
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            // 契约语义：Some = 存在可重试目标。
            return Ok(Some(_target.clone()));
        }
        if self
            .builder_retry_committed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            // 契约语义：None = 原启动已提交、只能继续观察。
            return Ok(None);
        }
        Err(ContainerRuntimeError::ConfigurationError(
            "Conditional builder retry is unsupported".into(),
        ))
    }

    async fn observe_builder_compute_running(
        &self,
        _target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self
            .builder_running_observed
            .load(std::sync::atomic::Ordering::SeqCst))
    }

    async fn apply_builder_control(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        // Record the physical control action against the captured workload
        // identity: handoff tests assert the successor acts on the residue's
        // EXACT uid, never a fabricated controller.
        if let Some(workload) = target.workload.as_ref() {
            self.builder_control_calls.lock().expect("lock").push((
                workload.uid.clone(),
                workload.name.clone(),
                restart,
            ));
        }
        Ok(None)
    }

    async fn observe_unbound_builder_creation_authority(
        &self,
        _context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<container_runtime_api::UnboundBuilderCreationAuthority> {
        if self
            .unbound_authority_transport_fails
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ContainerRuntimeError::K8sError(
                "injected unbound authority observation failure".into(),
            ));
        }
        Ok(self
            .unbound_creation_authority
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(container_runtime_api::UnboundBuilderCreationAuthority::Unsupported))
    }

    async fn verify_absent_builder_recovery(
        &self,
        _context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self
            .retained_creation_absent
            .load(std::sync::atomic::Ordering::SeqCst))
    }

    fn agent_workload_namespace(&self) -> String {
        "test-ns".into()
    }

    fn agent_workload_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        assert_eq!(*service_type, ServiceType::UserappBuilder);
        Ok(format!("rcoder-app-builder-{identifier}"))
    }

    async fn verify_agent_replacement(
        &self,
        _params: &container_runtime_api::ContainerCreateParams,
        _previous: Option<&ContainerBasicInfo>,
        _replacement: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<shared_types::VerifiedAgentReplacement> {
        self.agent_replacement_verifications
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .agent_replacement_transport_fails
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ContainerRuntimeError::K8sError(
                "injected verification transport failure".into(),
            ));
        }
        self.agent_replacement
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| {
                ContainerRuntimeError::Conflict("injected runtime workspace owner mismatch".into())
            })
    }

    async fn create_container(
        &self,
        _params: container_runtime_api::ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.builder_create_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    async fn builder_compute_absent(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<bool> {
        context
            .validate_identity(&context.app_id)
            .expect("valid captured context");
        self.builder_absence_probes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(message) = self.builder_absence_error.lock().unwrap().clone() {
            return Err(ContainerRuntimeError::K8sError(message));
        }
        Ok(self
            .builder_absent_proven
            .load(std::sync::atomic::Ordering::SeqCst))
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
            builder_startup_mode: Default::default(),
            resource_binding: None,
            context: _context.clone(),
            workload: None,
            pod: None,
            restart_image: None,
            restart_runtime_workspace: None,
        })
    }
    async fn capture_builder_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        let workload = self.builder_workload.lock().expect("lock").clone();
        Ok(BuilderControlTarget {
            builder_startup_mode: Default::default(),
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
            restart_runtime_workspace: None,
        })
    }
}

#[async_trait::async_trait]
impl WorkspaceRuntime for FenceRuntime {}

#[async_trait::async_trait]
impl UserAppDeploymentRuntime for FenceRuntime {
    async fn discover_retained_app_storage(
        &self,
        _app_id: &str,
        _scope: shared_types::UserAppOperationScope,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppRetainedStorage>> {
        Ok(self.retained_storage.lock().unwrap().clone())
    }

    async fn observe_app_operation_lease(
        &self,
        _context: &shared_types::UserAppExecutionContext,
        _receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<container_runtime_api::AppOperationLeaseObservation> {
        if let Some(reply) = self.hot_observation_replies.lock().unwrap().pop_front() {
            return reply;
        }
        Ok(self
            .hot_observation
            .lock()
            .unwrap()
            .unwrap_or(container_runtime_api::AppOperationLeaseObservation::Unconfirmed))
    }
    async fn exec_app_configuration_target(
        &self,
        _context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        if self.hot_target.lock().unwrap().as_ref() != Some(target) {
            return Err(ContainerRuntimeError::Conflict(
                "Original hot target changed".into(),
            ));
        }
        assert_eq!(command.len(), 3);
        assert_eq!(&command[..2], &["sh", "-c"]);
        assert!(command[2].contains("http://127.0.0.1:3010/v1/deploy/status"));
        assert!(!command[2].contains("--data"));
        self.hot_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let reply = self.hot_owner.lock().unwrap().clone().ok_or_else(|| {
            ContainerRuntimeError::ConnectionError("Original owner read unavailable".into())
        })?;
        Ok(container_runtime_api::ExecResult {
            exit_code: 0,
            stdout: reply.to_string(),
            stderr: String::new(),
        })
    }
    async fn app_env_snapshot(
        &self,
        _app_id: &str,
    ) -> ContainerRuntimeResult<shared_types::AppEnvSnapshot> {
        self.hot_env.lock().unwrap().clone().ok_or_else(|| {
            ContainerRuntimeError::ConnectionError(
                "Original hot environment read unavailable".into(),
            )
        })
    }
    async fn update_env_configmap_if_version(
        &self,
        _app_id: &str,
        _env: &HashMap<String, String>,
        _snapshot: &shared_types::AppEnvSnapshot,
    ) -> ContainerRuntimeResult<()> {
        self.hot_writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ContainerRuntimeError::Conflict(
            "hot env compare-and-swap rejected".into(),
        ))
    }
    async fn discover_application_identity(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppDiscoveredIdentity>> {
        // This protocol fixture has no native metadata discovery witnesses;
        // the caller's real lifecycle store remains the existing identity.
        Ok(None)
    }
    async fn capture_app_mutation_target(
        &self,
        _context: &shared_types::UserAppExecutionContext,
        _expected_resource_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::UserAppMutationTarget> {
        // 与 trait 默认一致: 本 fixture 不支持 identity-bound capture。
        Err(ContainerRuntimeError::ConfigurationError(
            "Identity-bound application stop is not supported by this runtime".into(),
        ))
    }
    async fn app_compute_absent(
        &self,
        _context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<bool> {
        self.app_absence_probes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(message) = self.app_absence_error.lock().unwrap().clone() {
            return Err(ContainerRuntimeError::K8sError(message));
        }
        // 开关打开时正向证实缺席, 模拟 image-roll 目标(Deployment/容器)已被删除。
        Ok(self
            .app_target_absent
            .load(std::sync::atomic::Ordering::SeqCst))
    }
    async fn prepare_compute_operation(
        &self,
        context: &shared_types::UserAppExecutionContext,
        scope: shared_types::UserAppOperationScope,
    ) -> ContainerRuntimeResult<Box<dyn shared_types::PreparedComputeLease>> {
        self.compute_prepares
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(PreparedComputeFixture(
            shared_types::UserAppOperationLeaseReceipt::Docker {
                service_type: shared_types::compute_lease_family(scope).unwrap(),
                device: 1,
                inode: 1,
                token: context.executor_id.clone(),
            },
        )))
    }
    async fn inspect_compute_drain_lease(
        &self,
        context: &shared_types::UserAppExecutionContext,
        scope: shared_types::UserAppOperationScope,
        receipt: Option<&shared_types::UserAppOperationLeaseReceipt>,
    ) -> ContainerRuntimeResult<shared_types::ComputeLeaseInspection> {
        let physical = self.physical_compute_runtime.lock().expect("lock").clone();
        if let Some(physical) = physical {
            return physical
                .inspect_compute_drain_lease(context, scope, receipt)
                .await;
        }
        if self.compute_stall.load(std::sync::atomic::Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if *self.receipt_verdict.lock().unwrap() == ReceiptVerdict::Transport {
            return Err(ContainerRuntimeError::K8sError(
                "injected observation failure".into(),
            ));
        }
        Ok(self
            .compute_inspection
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| {
                receipt.map_or(shared_types::ComputeLeaseInspection::Absent, |r| {
                    shared_types::ComputeLeaseInspection::Releasable(r.clone())
                })
            }))
    }

    async fn prepare_compute_handoff(
        &self,
        context: &shared_types::UserAppExecutionContext,
        scope: shared_types::UserAppOperationScope,
        receipt: Option<&shared_types::UserAppOperationLeaseReceipt>,
    ) -> ContainerRuntimeResult<container_runtime_api::ComputeHandoff> {
        use container_runtime_api::ComputeHandoff;
        use shared_types::ComputeLeaseInspection;
        let physical = self.physical_compute_runtime.lock().expect("lock").clone();
        if let Some(physical) = physical {
            return physical
                .prepare_compute_handoff(context, scope, receipt)
                .await;
        }
        match self
            .inspect_compute_drain_lease(context, scope, receipt)
            .await?
        {
            ComputeLeaseInspection::Held => Ok(ComputeHandoff::Held),
            observed @ (ComputeLeaseInspection::IdentityChanged(_)
            | ComputeLeaseInspection::PreviousOwner { .. }
            | ComputeLeaseInspection::PreviousOperation { .. }) => {
                Ok(ComputeHandoff::deferred(observed))
            }
            ComputeLeaseInspection::Discovered { receipt, attempt }
                if attempt.executor_id != context.executor_id =>
            {
                Ok(ComputeHandoff::deferred(
                    ComputeLeaseInspection::Discovered { receipt, attempt },
                ))
            }
            ComputeLeaseInspection::Absent
            | ComputeLeaseInspection::Releasable(_)
            | ComputeLeaseInspection::Discovered { .. } => Ok(ComputeHandoff::Acquired {
                guard: Box::new(
                    crate::userapp_builder::compute_drain_recovery::UnheldComputeFixture,
                ),
                original_absent: false,
            }),
        }
    }
    async fn release_app_operation_receipt(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        let physical = self.physical_compute_runtime.lock().expect("lock").clone();
        if let Some(physical) = physical {
            return physical
                .release_app_operation_receipt(context, receipt)
                .await;
        }
        self.compute_releases
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .compute_release_succeeds
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            Ok(())
        } else {
            Err(ContainerRuntimeError::K8sError(
                "injected lease release failure".into(),
            ))
        }
    }
    async fn release_previous_compute_lease(
        &self,
        current: &shared_types::UserAppExecutionContext,
        scope: shared_types::UserAppOperationScope,
        owner: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        assert_eq!(
            receipt.service_type(),
            &shared_types::compute_lease_family(scope).unwrap()
        );
        self.previous_compute_releases
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.previous_compute_arguments.lock().unwrap().push((
            current.clone(),
            owner.clone(),
            receipt.clone(),
        ));
        if self
            .compute_release_succeeds
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            let mut inspection = self.compute_inspection.lock().unwrap();
            let exact = match inspection.as_ref() {
                Some(shared_types::ComputeLeaseInspection::PreviousOwner {
                    owner: observed,
                    receipt: captured,
                }) => observed == owner && captured == receipt,
                Some(shared_types::ComputeLeaseInspection::PreviousOperation {
                    operation_id,
                    receipt: captured,
                }) => operation_id == &owner.operation_id && captured == receipt,
                None | Some(shared_types::ComputeLeaseInspection::Absent) => true,
                Some(
                    shared_types::ComputeLeaseInspection::Held
                    | shared_types::ComputeLeaseInspection::IdentityChanged(_)
                    | shared_types::ComputeLeaseInspection::Releasable(_)
                    | shared_types::ComputeLeaseInspection::Discovered { .. },
                ) => false,
            };
            if exact {
                // A successful exact release really removes the old mutex;
                // subsequent pre-fence observation must not see stale residue.
                *inspection = None;
                Ok(())
            } else {
                Err(ContainerRuntimeError::Conflict(
                    "previous mutex changed before release".into(),
                ))
            }
        } else {
            Err(ContainerRuntimeError::Conflict(
                "previous lease changed during conditional release".into(),
            ))
        }
    }
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
        let physical = self.physical_compute_runtime.lock().expect("lock").clone();
        if let Some(physical) = physical {
            return physical
                .app_operation_receipt_holder_dead(context, receipt)
                .await;
        }
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
) -> (
    Arc<AppState>,
    Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
) {
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
    fence_state_on_store(
        runtime,
        kind,
        operation_id,
        store,
        None,
        None,
        TerminalSeed::fence(UserAppOperationState::RecoveryRequired),
    )
    .await
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
    let state = runtime_state(runtime, store.clone(), fence_settle_grace_secs).await;
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
                serving_recovery: false,
            }),
            true,
        ),
        UserAppOperationKind::RestartDeployment => (
            Some(UserAppControlCommand::Deploy {
                restart: true,
                input_digest: digest,
                serving_recovery: false,
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
    seed_terminal_fence(
        &store,
        &running,
        seed.state,
        seed.step,
        checkpoint,
        seed.error_message,
    )
    .await;
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
    message: Option<&str>,
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
                message.unwrap_or("Capture explicit database target: Owned management container is not running").into(),
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

/// Deployment absence alone cannot authorize retiring a deletion whose original
/// captured targets are unavailable: a detached dev/storage write may remain.
#[tokio::test]
async fn delete_compute_absence_does_not_retire_unknown_deletion() {
    let operation_id = "op-delete-absent";
    let runtime = FenceRuntime::scenario(None, None);
    let (state, _dir) =
        fence_state(runtime, UserAppOperationKind::DeleteCompute, operation_id).await;
    let snapshot = settled(&state, operation_id).await;
    let error = reconcile_fenced_ensure(&state, &snapshot)
        .await
        .expect_err("deployment absence must not retire unknown deletion writes");
    assert!(
        error
            .to_string()
            .contains("Original deletion targets are unavailable")
    );
    let record = settled(&state, operation_id).await;
    assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
    assert!(record.checkpoint.get("fence_released_evidence").is_none());
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

/// Shared real-store AppState fixture for lifecycle recovery protocols.
pub(crate) async fn runtime_state(
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    fence_settle_grace_secs: Option<u64>,
) -> Arc<AppState> {
    runtime_state_with_mode(
        runtime,
        store,
        fence_settle_grace_secs,
        app_manager::config::AppAccessMode::Docker,
    )
    .await
}

pub(crate) async fn runtime_state_with_mode(
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    fence_settle_grace_secs: Option<u64>,
    access_mode: app_manager::config::AppAccessMode,
) -> Arc<AppState> {
    runtime_state_with_config(
        runtime,
        store,
        crate::config::AppConfig {
            fence_settle_grace_secs,
            ..Default::default()
        },
        access_mode,
    )
    .await
}

/// 带完整 [`crate::config::AppConfig`] 覆盖的 AppState 夹具：陈旧 Running
/// 对账的确认年龄门需要覆盖 `ensure_timeout_seconds`。
pub(crate) async fn runtime_state_with_config(
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    config: crate::config::AppConfig,
    access_mode: app_manager::config::AppAccessMode,
) -> Arc<AppState> {
    let adapter =
        crate::storage::ProjectAdapter::new("test-ns".to_string(), "cluster.local".to_string());
    let activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
        300,
    )));
    let manager_config = app_manager::config::AppManagerConfig {
        access_mode,
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
    Arc::new(AppState {
        userapp_store: store.clone(),
        userapp_store_control: store.clone(),
        userapp_op_flight: Arc::new(
            crate::userapp_builder::shutdown_gate::OperationFlightGate::default(),
        ),
        userapp_recovery_handle: Arc::new(Mutex::new(None)),
        config,
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

        agent_download_manager: Arc::new(
            agent_provisioning::AgentDownloadManager::new(download_dir.path())
                .expect("download manager"),
        ),
        app_service,
        activity,
        cluster_domain: "cluster.local".to_string(),
    })
}

/// 陈旧 Running/claimed 夹具（生产 app 50/128 形态）：claim 后不落任何
/// 终态/中断记录——无 deadline、无租约绑定、无完成回执，执行者死亡后
/// 无人再转换状态。`ensure_timeout_secs` 覆盖确认年龄门。
pub(crate) async fn stale_running_state(
    runtime: Arc<FenceRuntime>,
    operation_id: &str,
    ensure_timeout_secs: u64,
) -> (Arc<AppState>, tempfile::TempDir) {
    runtime
        .compute_release_succeeds
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let metadata_dir = tempfile::tempdir().expect("metadata directory");
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &metadata_dir.path().join("userapp.turso.db"),
        )
        .await
        .expect("store"),
    );
    let mut config = crate::config::AppConfig::default();
    config.userapp_storage.ensure_timeout_seconds = ensure_timeout_secs;
    let state = runtime_state_with_config(
        runtime.clone(),
        store.clone(),
        config,
        app_manager::config::AppAccessMode::Docker,
    )
    .await;
    let lifecycle = store.ensure_identity("fenced").await.expect("identity");
    let admitted = store
        .admit_with_input(
            &UserAppAdmission {
                app_id: "fenced".into(),
                lifecycle_id: Some(lifecycle.lifecycle_id.clone()),
                operation_id: operation_id.into(),
                request_id: Some(format!("req-{operation_id}")),
                request_fingerprint: "ab".repeat(32),
                kind: UserAppOperationKind::EnsureBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            },
            None,
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
    // TempDir 必须活过测试：把目录泄露（测试进程级可接受）。
    std::mem::forget(metadata_dir);
    (state, tempfile::tempdir().expect("placeholder dir"))
}

struct PreparedComputeFixture(shared_types::UserAppOperationLeaseReceipt);
#[async_trait::async_trait]
impl shared_types::AppOperationLease for PreparedComputeFixture {
    fn receipt(&self) -> Option<shared_types::UserAppOperationLeaseReceipt> {
        Some(self.0.clone())
    }
    async fn release(self: Box<Self>) -> Result<(), String> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl shared_types::PreparedComputeLease for PreparedComputeFixture {
    async fn activate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn default_recovery_settles_acknowledged_legacy_identity_rejection_only_with_absent_execution()
 {
    for confirmed_absent in [false, true] {
        let runtime = FenceRuntime::scenario(None, None);
        runtime
            .retained_creation_absent
            .store(confirmed_absent, std::sync::atomic::Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &dir.path().join("legacy-fence.db"),
            )
            .await
            .unwrap(),
        );
        let (state,_) = fence_state_on_store(runtime,UserAppOperationKind::EnsureBuilder,"legacy-preflight",store,None,None,
            TerminalSeed {state:UserAppOperationState::RecoveryRequired,step:"creation_result",checkpoint:Some(serde_json::Value::Null),
                error_message:Some("ensure UserappBuilder failed: Concurrent resource update: Application resource identity mismatch: rcoder.io/application-id")}).await;
        let snapshot = settled(&state, "legacy-preflight").await;
        reconcile_default_recovery_before_ensure(
            &state,
            &state
                .userapp_store
                .get_application("fenced")
                .await
                .unwrap()
                .unwrap(),
        )
        .await
        .unwrap();
        let after = settled(&state, "legacy-preflight").await;
        if confirmed_absent {
            assert_eq!(after.state, UserAppOperationState::Failed);
            assert_eq!(after.operation_id, snapshot.operation_id);
            assert!(
                state
                    .userapp_store
                    .get_application("fenced")
                    .await
                    .unwrap()
                    .unwrap()
                    .active_operations
                    .dev
                    .is_none()
            );
            assert_eq!(
                after.checkpoint["fence_released_evidence"]["kind"],
                "legacy_creation_preflight_rejected_compute_absent"
            );
        } else {
            assert_eq!(after.state, UserAppOperationState::RecoveryRequired);
        }
    }
}

#[tokio::test]
async fn default_recovery_metadata_claim_reconciles_but_unknown_generation_write_remains_fenced() {
    for stage in [
        container_runtime_api::CreationStage::StorageClaim,
        container_runtime_api::CreationStage::WriteGeneration,
    ] {
        let runtime = FenceRuntime::scenario(None, None);
        runtime
            .retained_creation_absent
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &dir.path().join("claim.db"),
            )
            .await
            .unwrap(),
        );
        let app = store.ensure_identity("fenced").await.unwrap();
        let captured = shared_types::UserAppRetainedStorage {
            app_id: "fenced".into(),
            scope: shared_types::UserAppOperationScope::Dev,
            lifecycle_id: Some(app.lifecycle_id),
            services: vec![],
            volumes: vec![AppResourceIdentity {
                kind: AppResourceKind::PersistentVolumeClaim,
                name: "original".into(),
                uid: "original-uid".into(),
                resource_version: None,
            }],
        };
        *runtime.retained_storage.lock().unwrap() = Some(captured.clone());
        let (state,_)=fence_state_on_store(runtime,UserAppOperationKind::EnsureBuilder,"metadata-claim",store,None,None,
            TerminalSeed {state:UserAppOperationState::RecoveryRequired,step:"creation_result",error_message:Some("metadata reply lost"),
                checkpoint:Some(serde_json::json!({"retained_storage":captured,"creation_abort":container_runtime_api::CreationProgress {failed_at:stage,definitive_rejection:false,retained_idempotent_resources:vec!["original volume".into()]}}))}).await;
        let record = settled(&state, "metadata-claim").await;
        reconcile_fenced_ensure(&state, &record).await.unwrap();
        let after = settled(&state, "metadata-claim").await;
        assert_eq!(
            after.state,
            if stage == container_runtime_api::CreationStage::StorageClaim {
                UserAppOperationState::Failed
            } else {
                UserAppOperationState::RecoveryRequired
            }
        );
    }
}
