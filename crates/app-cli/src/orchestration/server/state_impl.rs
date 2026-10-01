use super::*;

impl ServerState {
    pub fn new(ready: RuntimeStatusService) -> Self {
        let (deploy_tx, deploy_rx) = tokio::sync::mpsc::unbounded_channel();
        let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
        let (supervision_probe_tx, supervision_probe_rx) = tokio::sync::mpsc::channel(4);
        Self {
            control_token: std::sync::OnceLock::new(),
            execution_project: std::sync::OnceLock::new(),
            owner_execution_workspace: std::sync::OnceLock::new(),
            runtime_kernel: std::sync::OnceLock::new(),
            kernel_required: std::sync::atomic::AtomicBool::new(false),
            runtime_recovery_hold: std::sync::atomic::AtomicU8::new(0),
            credential_recovery_operation: std::sync::Mutex::new(None),
            pending_dev_profile: std::sync::Mutex::new(None),
            proxy_context: RwLock::new(None),
            pending_run_config: std::sync::Mutex::new(None),
            control_tx,
            control_rx: tokio::sync::Mutex::new(control_rx),
            supervision_driver_started: std::sync::atomic::AtomicBool::new(false),
            supervision_probe_tx,
            supervision_probe_rx: tokio::sync::Mutex::new(supervision_probe_rx),
            current_runtime_operation: RwLock::new(None),
            business_generation: RwLock::new(None),
            native_control: std::sync::OnceLock::new(),
            admission: std::sync::Mutex::new(()),
            accepting: std::sync::atomic::AtomicBool::new(true),
            auxiliary_writers: std::sync::atomic::AtomicUsize::new(0),
            shutdown_unconfirmed: std::sync::atomic::AtomicBool::new(false),
            shutdown_grace_seconds: std::sync::atomic::AtomicU64::new(30),
            shutdown_group_count: std::sync::atomic::AtomicU64::new(1),
            initializing: std::sync::atomic::AtomicBool::new(true),
            preparations: Arc::new(preparation::Preparations::default()),
            journal: std::sync::Mutex::new(None),
            volatile_deploy_replays: std::sync::Mutex::new(Default::default()),
            generation: RwLock::new(
                std::env::var(shared_types::APP_DEPLOY_GENERATION_ID)
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            ),
            phase: RwLock::new(ServerPhase::Idle),
            release: RwLock::new(None),
            ready,
            deploy_status: RwLock::new(DeployStatus {
                phase: AppCliDeployPhase::Idle,
                protocol_version: DEPLOY_PROTOCOL,
                capabilities: vec![
                    "progress_v1".into(),
                    "deployment_run_pg".into(),
                    shared_types::BUSINESS_READINESS_CAPABILITY.into(),
                ],
                ..Default::default()
            }),
            deploy_tx,
            deploy_rx: tokio::sync::Mutex::new(deploy_rx),
            cancel: RwLock::new(CancellationToken::new()),
            log_layout: RwLock::new(LogLayout::Builtin),
            deploy_inputs_eligible: std::sync::atomic::AtomicBool::new(true),
            business_relaunch: std::sync::OnceLock::new(),
            business_recovery_active: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 运行操作内核（api 层 /v1/runtime/* 消费；未注入返回 None）。
    pub(crate) fn mark_kernel_required(&self) {
        self.kernel_required
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// R08：记录/读取当前操作的 dev profile。
    pub(crate) fn set_pending_dev_profile(&self, dev_profile: bool) {
        *self
            .pending_dev_profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dev_profile);
    }

    pub(crate) fn take_pending_dev_profile(&self) -> Option<bool> {
        self.pending_dev_profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub(crate) fn set_proxy_context(&self, workspace: std::path::PathBuf, dev_profile: bool) {
        *self
            .proxy_context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(crate::proxy::compiler::RuntimeProxyContext {
                workspace,
                dev_profile,
            });
    }

    pub(crate) fn proxy_context(&self) -> Option<crate::proxy::compiler::RuntimeProxyContext> {
        self.proxy_context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_pending_run_config(&self, pg: Option<shared_types::StartPgCredential>) {
        *self
            .pending_run_config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = pg;
    }

    pub(crate) fn take_pending_run_config(&self) -> Option<shared_types::StartPgCredential> {
        self.pending_run_config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// V04：终态持久化失败（结果未知）——挂起 server 写入口与 ready。
    pub(crate) fn begin_runtime_recovery_hold(&self) {
        self.runtime_recovery_hold
            .fetch_or(2, std::sync::atomic::Ordering::AcqRel);
        self.ready.set_ready(false);
    }

    pub(crate) fn runtime_recovery_hold_active(&self) -> bool {
        self.runtime_recovery_hold
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    pub(super) fn credentials_only_hold(&self) -> bool {
        self.runtime_recovery_hold
            .load(std::sync::atomic::Ordering::Acquire)
            == 1
    }

    pub(super) fn begin_credentials_hold(&self) {
        self.runtime_recovery_hold
            .fetch_or(1, std::sync::atomic::Ordering::AcqRel);
        self.ready.set_ready(false);
    }

    pub(super) fn consume_credentials_hold(&self, operation_id: &str) -> Result<()> {
        let mut owner = self
            .credential_recovery_operation
            .lock()
            .map_err(|_| anyhow::anyhow!("credential recovery identity lock poisoned"))?;
        anyhow::ensure!(owner.is_none(), "credential recovery is already executing");
        self.runtime_recovery_hold
            .compare_exchange(
                1,
                0,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| anyhow::anyhow!("runtime recovery changed during credential admission"))?;
        *owner = Some(operation_id.to_owned());
        Ok(())
    }

    /// A failure restores only this request's credential fence. Stop and late
    /// callbacks belonging to other operations cannot consume its identity.
    pub(super) fn settle_credential_recovery(&self, operation_id: &str, running: bool) {
        let Ok(mut owner) = self.credential_recovery_operation.lock() else {
            self.begin_runtime_recovery_hold();
            tracing::error!("credential recovery identity lock poisoned");
            return;
        };
        if owner.as_deref() == Some(operation_id) {
            if !running {
                self.begin_credentials_hold();
            }
            *owner = None;
        }
    }

    /// Called only by the execution loop after all business processes stopped.
    /// A legacy deployment has no kernel terminal callback to restore its fence.
    pub(super) fn restore_credentials_after_stop(&self) {
        let Ok(mut owner) = self.credential_recovery_operation.lock() else {
            self.begin_runtime_recovery_hold();
            tracing::error!("credential recovery identity lock poisoned during stop");
            return;
        };
        if owner.take().is_some() {
            self.begin_credentials_hold();
        }
    }

    /// A fresh explicit operation may supply missing startup credentials
    /// only after verifying the previously confirmed artifact. This is not reconciliation
    /// of an interrupted operation (the kernel retains that separate fence).
    pub(crate) fn can_supply_run_credentials(
        &self,
        pg: Option<&shared_types::StartPgCredential>,
        source_only: bool,
    ) -> Result<bool> {
        if !self.credentials_only_hold()
            || self
                .shutdown_unconfirmed
                .load(std::sync::atomic::Ordering::Acquire)
            || !pg.is_some_and(|pg| !pg.username.trim().is_empty() && !pg.password.is_empty())
        {
            return Ok(false);
        }
        let receipt = self
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
            .as_ref()
            .and_then(|journal| journal.receipt.clone())
            .context("confirmed deployment journal missing")?;
        let confirmed_boundary = matches!(
            receipt.boundary,
            Boundary::Active | Boundary::RestoredActive | Boundary::StartupFailed
        ) || (receipt.boundary == Boundary::Preparing
            && receipt.operation.phase == AppCliDeployPhase::Failed);
        if receipt.generation != self.generation_value() || !confirmed_boundary {
            return Ok(false);
        }
        let active = receipt
            .active
            .context("confirmed active artifact missing")?;
        let Some(request) = active.request.as_ref() else {
            return Ok(false);
        };
        if source_only && request.execution_target != Some(ExecutionTarget::Source) {
            return Ok(false);
        }
        let workspace = if let Some(target) = request.execution_target {
            let project = self
                .execution_project
                .get()
                .context("owner project missing")?;
            resolved_execution_workspace(project, Some(target), self)?
        } else {
            // Legacy URL deployment used the owner's configured directory.
            // Never infer missing local-artifact provenance from this fallback.
            if request.local_path.is_some()
                || !(request.url.starts_with("https://") || request.url.starts_with("http://"))
            {
                return Ok(false);
            }
            self.owner_execution_workspace
                .get()
                .context("owner execution workspace missing")?
                .clone()
        };
        crate::migration_journal::require_confirmed_migrations(&workspace)?;
        let release = crate::manifest::read_release_lock(&workspace)?;
        Ok(release.release_id == active.artifact_release_id)
    }

    pub(crate) async fn recovery_view(&self) -> Result<shared_types::RuntimeRecoveryView> {
        let kernel = self
            .runtime_kernel()
            .context("runtime kernel is unavailable")?;
        let status = kernel.status().await?;
        let journal = self
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
        let saved_receipt = journal.as_ref().and_then(|journal| journal.receipt.clone());
        drop(journal);
        let receipt = saved_receipt.as_ref();
        let boundary = receipt.map(|receipt| {
            match receipt.boundary {
                Boundary::Preparing => "preparing",
                Boundary::Switching => "switching",
                Boundary::Activated => "activated",
                Boundary::Active => "active",
                Boundary::RestoredActive => "restored_active",
                Boundary::StartupFailed => "startup_failed",
                Boundary::Failed => "failed",
            }
            .to_owned()
        });
        let migrations = receipt
            .filter(|receipt| receipt.generation == self.generation_value())
            .and_then(|receipt| receipt.active.as_ref())
            .and_then(|active| active.request.as_ref())
            .and_then(|request| request.execution_target)
            .map(|target| {
                let workspace = execution_workspace(
                    std::path::Path::new(&kernel.identity().source_root),
                    Some(target),
                );
                match crate::migration_journal::inspect_migrations(&workspace) {
                    Ok(true) => shared_types::RuntimeMigrationRecoveryState::Confirmed,
                    Ok(false) => shared_types::RuntimeMigrationRecoveryState::Unconfirmed,
                    Err(_) => shared_types::RuntimeMigrationRecoveryState::Unreadable,
                }
            })
            .unwrap_or_default();
        Ok(shared_types::RuntimeRecoveryView {
            runtime_instance_id: status.runtime_instance_id,
            deployment_generation_id: self.generation_value(),
            revision: status.revision,
            kernel_protected: status.recovery_protection,
            owner_protected: self.runtime_recovery_hold_active(),
            operation_id: receipt.map(|receipt| receipt.operation.operation_id.clone()),
            boundary,
            generation_matches: receipt
                .map(|receipt| receipt.generation == self.generation_value()),
            credentials_required: receipt
                .and_then(|receipt| receipt.active.as_ref())
                .and_then(|active| active.request.as_ref())
                .and_then(|request| request.run_pg.as_ref())
                .is_some_and(|pg| pg.password.is_empty()),
            migrations,
        })
    }

    pub(crate) fn kernel_unavailable(&self) -> bool {
        self.kernel_required
            .load(std::sync::atomic::Ordering::Acquire)
            && self.runtime_kernel.get().is_none()
    }

    pub(crate) fn runtime_kernel(&self) -> Option<Arc<crate::runtime_kernel::RuntimeKernel>> {
        self.runtime_kernel.get().cloned()
    }

    /// 注入运行操作内核（serve 在 ownership 认领后调用；幂等拒绝二次注入）。
    pub(crate) fn set_runtime_kernel(
        &self,
        kernel: Arc<crate::runtime_kernel::RuntimeKernel>,
    ) -> bool {
        self.runtime_kernel.set(kernel).is_ok()
    }

    /// 当前执行中的运行操作（dispatch 设置；主循环边界收束后清除）。
    ///
    /// 语义（R01 修复）：这是**正在执行**的操作身份，只在为空时由新动作占据；
    /// Stop 受理（active 期间允许）不覆盖在执行身份——停止操作由主循环在
    /// 边界显式执行并按自身 ID 收束（见 [`Self::finish_runtime_operation_by_id`]）。
    pub(crate) fn set_current_runtime_operation(&self, operation_id: Option<String>) {
        let mut guard = self
            .current_runtime_operation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 仅允许 占空→有值 或 清空；有值被覆盖（迟到启动/停止并发）拒绝。
        if guard.is_some() && operation_id.is_some() {
            tracing::warn!(
                existing = ?guard,
                incoming = ?operation_id,
                "runtime operation identity is executing; refusing to overwrite (R01)"
            );
            return;
        }
        *guard = operation_id;
    }

    pub(crate) fn current_runtime_operation(&self) -> Option<String> {
        self.current_runtime_operation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 部署请求 token（`request_release_id`）——业务就绪观察用来标注
    /// 「正在准备的目标版本」。与 release_id（内容身份）两层语义。
    pub(crate) fn deploy_request_release_id(&self) -> Option<String> {
        self.deploy_status
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request_release_id
            .clone()
    }

    /// 按显式 ID 收束运行操作（R01）：只收束指定操作，不误完成并发受理的
    /// 其他操作；在执行槽匹配时一并清除。
    ///
    /// V04：终态**持久化失败向上传播**——保留执行身份（不清 current）、
    /// 挂起 server 恢复门禁（写入口关闭 + ready 压低），调用方必须显式处理；
    /// 不允许记日志后按成功继续。
    pub(crate) async fn finish_runtime_operation_by_id(
        &self,
        operation_id: &str,
        state: shared_types::RuntimeOperationState,
        error: Option<(String, String)>,
    ) -> Result<(), String> {
        let Some(kernel) = self.runtime_kernel() else {
            return Ok(());
        };
        if state != shared_types::RuntimeOperationState::Succeeded {
            // Restore before kernel.finish can release admission to another request.
            self.settle_credential_recovery(operation_id, false);
        }
        if let Err(persist_error) = kernel.finish(operation_id, state, error, None, 0).await {
            let message = format!(
                "runtime operation terminal persist failed (op {operation_id}): {persist_error:#}"
            );
            tracing::error!("{message}");
            self.begin_runtime_recovery_hold();
            return Err(message);
        }
        if self.current_runtime_operation().as_deref() == Some(operation_id) {
            self.set_current_runtime_operation(None);
        }
        Ok(())
    }

    /// 当前执行操作是否已被请求取消（编排完成提交边界检查）。
    pub(crate) fn current_operation_cancelled(&self) -> bool {
        let Some(operation_id) = self.current_runtime_operation() else {
            return false;
        };
        self.runtime_kernel()
            .is_some_and(|kernel| kernel.is_cancelled(&operation_id))
    }

    /// 取消检查点（R03）：操作在执行副作用开始前已被取消 → 直接收束为
    /// Cancelled（无副作用，无需清理），返回 true（调用方跳过执行）。
    pub(crate) async fn settle_cancelled_before_execution(&self, operation_id: &str) -> bool {
        let Some(kernel) = self.runtime_kernel() else {
            return false;
        };
        if !kernel.is_cancelled(operation_id) {
            return false;
        }
        tracing::info!("runtime operation {operation_id} cancelled before execution");
        if let Err(error) = self
            .finish_runtime_operation_by_id(
                operation_id,
                shared_types::RuntimeOperationState::Cancelled,
                None,
            )
            .await
        {
            // V04：零副作用取消的终态都写不进——结果不可记录，写入口挂起
            tracing::error!("{error}; suppressing further execution");
        }
        true
    }

    /// 主循环边界收束运行操作（成功/失败/恢复保护三态；先持久化终态再清槽）。
    /// V04：持久化失败向上传播（身份保留 + 门禁已在底层挂起）。
    pub(crate) async fn finish_current_runtime_operation(
        &self,
        state: shared_types::RuntimeOperationState,
        error: Option<(String, String)>,
    ) -> Result<(), String> {
        let Some(operation_id) = self.current_runtime_operation() else {
            return Ok(());
        };
        self.finish_runtime_operation_by_id(&operation_id, state, error)
            .await
    }

    pub(super) fn close_admission(&self) {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.accepting
            .store(false, std::sync::atomic::Ordering::Release);
        self.trigger_cancel();
    }

    pub(super) fn generation_control(
        self: &Arc<Self>,
        generation: &str,
    ) -> Result<Arc<dyn runtime_supervisor::WorkerControl>> {
        let _admission = self
            .admission
            .lock()
            .map_err(|_| anyhow::anyhow!("runtime admission lock poisoned"))?;
        *self
            .business_generation
            .write()
            .map_err(|_| anyhow::anyhow!("business generation lock poisoned"))? =
            Some(generation.to_owned());
        self.initializing
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(Arc::new(super::state::GenerationControl {
            state: self.clone(),
            generation: generation.to_owned(),
        }))
    }

    pub(super) fn set_native_control(
        &self,
        observe: impl Fn() -> Option<(String, String)> + Send + Sync + 'static,
    ) -> Result<()> {
        self.native_control
            .set(Box::new(observe))
            .map_err(|_| anyhow::anyhow!("native control observer already installed"))
    }

    pub(crate) fn native_control_blocker(&self) -> Option<(String, String)> {
        self.native_control.get().and_then(|observe| observe())
    }

    /// 统一 owner：注入业务重启通知（运行操作受理后触发会话重建）。
    pub(crate) fn set_business_relaunch(
        &self,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<()> {
        self.business_relaunch
            .set(Box::new(notify))
            .map_err(|_| anyhow::anyhow!("business relaunch hook already installed"))
    }

    /// 启动恢复是否仍在进行（P1-01：API bind 先于恢复，写端点/ready 门控依据）。
    pub fn initializing(&self) -> bool {
        self.initializing.load(std::sync::atomic::Ordering::Acquire)
    }

    /// 统一 owner（recovery v2）：进入新的业务会话。重置会话级运行面（代次、
    /// 取消令牌、部署受理、journal 槽位、相位与部署进度），保留跨会话事实
    /// （owner token、运行内核、recovery hold、凭据恢复标记）。
    /// `fresh=false` 的恢复式会话不消费一次性部署声明 env（等效进程模式在
    /// 派生时剥除 APP_DEPLOY_* 的语义）。
    ///
    /// RV04：会话身份、取消令牌更新与 Stop 受理共用 admission 线性化点。
    /// `stop_handover_in_progress` 在锁内探测 durable 停止交接状态（原生
    /// Stop/Shutdown 已受理且未终态）——满足且上一代令牌已取消时接力；
    /// 终态（Stopped）后的空闲管理会话取新令牌，不构成停止循环。Stop 的
    /// 取消侧（WorkerControl::shutdown）同样先取 admission 再取消，两侧
    /// 在同一临界区内排序，renew 不再丢弃已受理的停止。
    pub(crate) fn begin_business_session(
        &self,
        generation: String,
        fresh: bool,
        stop_handover_in_progress: impl FnOnce() -> bool,
    ) {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stop_in_progress = stop_handover_in_progress();
        *self
            .business_generation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(generation.clone());
        let cancelled_before = self.cancel_token().is_cancelled();
        self.renew_cancel_locked();
        if stop_in_progress && cancelled_before {
            self.trigger_cancel();
        }
        self.initializing
            .store(true, std::sync::atomic::Ordering::Release);
        self.accepting
            .store(true, std::sync::atomic::Ordering::Release);
        self.shutdown_unconfirmed
            .store(false, std::sync::atomic::Ordering::Release);
        self.supervision_driver_started
            .store(false, std::sync::atomic::Ordering::Release);
        self.deploy_inputs_eligible
            .store(fresh, std::sync::atomic::Ordering::Release);
        *self
            .generation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = generation;
        *self
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .current_runtime_operation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .pending_dev_profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .pending_run_config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .release
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = DeployStatus {
            phase: AppCliDeployPhase::Idle,
            protocol_version: DEPLOY_PROTOCOL,
            capabilities: vec![
                "progress_v1".into(),
                "deployment_run_pg".into(),
                shared_types::BUSINESS_READINESS_CAPABILITY.into(),
            ],
            ..Default::default()
        };
        self.set_phase_locked(ServerPhase::Idle);
    }

    pub(crate) fn begin_auxiliary_write(&self) -> Result<AuxiliaryWriter<'_>> {
        let _admission = self
            .admission
            .lock()
            .map_err(|_| anyhow::anyhow!("runtime admission lock poisoned"))?;
        anyhow::ensure!(
            self.accepting.load(std::sync::atomic::Ordering::Acquire)
                && !self.initializing()
                && !self.runtime_recovery_hold_active(),
            "runtime write admission is closed"
        );
        anyhow::ensure!(
            self.auxiliary_writers
                .load(std::sync::atomic::Ordering::Acquire)
                == 0,
            "another runtime configuration writer is active"
        );
        self.auxiliary_writers
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(AuxiliaryWriter {
            state: self,
            confirmed: false,
        })
    }

    /// 标记启动恢复完成：开放写端点受理与 ready 判定。
    /// legacy 直跑形态在 API bind 成功后立即调用（无恢复窗口）。
    pub fn mark_initialized(&self) {
        self.initializing
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub fn phase(&self) -> ServerPhase {
        self.phase
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_phase(&self, phase: ServerPhase) {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.set_phase_locked(phase);
    }

    pub(super) fn set_phase_locked(&self, phase: ServerPhase) {
        // Ordinary runtime transitions cannot prove that an unconfirmed writer
        // stopped. Keep admission closed until explicit shutdown reconciliation.
        if self
            .deploy_status()
            .operation
            .as_ref()
            .and_then(|operation| operation.recovery.as_ref())
            .is_some_and(|status| status.status == "pending")
        {
            return;
        }
        let mut guard = self
            .phase
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = phase.clone();
        drop(guard);
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.phase = AppCliDeployPhase::from(&phase);
        if let ServerPhase::Failed(err) = &phase {
            status.error = Some(err.clone());
        }
        if let Some(op) = &mut status.operation
            && op.phase != AppCliDeployPhase::Failed
        {
            op.phase = AppCliDeployPhase::from(&phase);
            if let ServerPhase::Failed(error) = &phase {
                op.error = Some(error.clone());
                if op.deploy_stage == AppDeploymentStage::Pending {
                    op.deploy_stage = AppDeploymentStage::Failed;
                }
            }
        }
    }

    /// 更新部署进度（progress_v1 能力协议）。
    pub(crate) fn set_deploy_progress(&self, progress: shared_types::AppDeploymentProgress) {
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.progress = Some(progress);
    }

    /// 清除部署进度（部署结束/重启时重置）。
    #[allow(dead_code)] // 由部署结束/重启路径消费（batch 2b/2c 进度协议）
    pub(crate) fn clear_deploy_progress(&self) {
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.progress = None;
    }

    pub(super) fn begin_failure(&self, error: String, shutdown_unconfirmed: bool) {
        if shutdown_unconfirmed {
            self.shutdown_unconfirmed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Publish failure and shutdown uncertainty atomically. The retained
        // recovery.pending wire value is a quiescence fence, not rollback intent.
        let phase = if shutdown_unconfirmed {
            ServerPhase::Orchestrating
        } else {
            ServerPhase::Failed(error.clone())
        };
        *self
            .phase
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = phase.clone();
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.phase = AppCliDeployPhase::from(&phase);
        status.error = Some(error.clone());
        if let Some(operation) = &mut status.operation {
            operation.phase = AppCliDeployPhase::Failed;
            operation.error = Some(error);
            if operation.deploy_stage == AppDeploymentStage::Pending {
                operation.deploy_stage = AppDeploymentStage::Failed;
            }
            operation.recovery =
                shutdown_unconfirmed.then(|| shared_types::AppDeploymentRecovery {
                    status: "pending".into(),
                    error: None,
                    database_migrations_reversed: false,
                });
        }
    }

    /// 当前 release（部署编排成功后置入；幂等恢复路径直接从 lock 文件读入）。
    pub fn release(&self) -> Option<ReleaseLock> {
        self.release
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn control_token(&self) -> Option<String> {
        self.control_token.get().cloned().or_else(|| {
            std::env::var("APP_CLI_DEPLOY_TOKEN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
    }

    pub(crate) fn initialize_owner_token(&self) -> Result<()> {
        let token = self
            .control_token()
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        self.control_token
            .set(token)
            .map_err(|_| anyhow::anyhow!("owner control token already initialized"))
    }

    pub fn set_release(&self, release: ReleaseLock) {
        let enabled = release.services.iter().filter(|service| service.enabled);
        self.shutdown_grace_seconds.fetch_max(
            enabled
                .clone()
                .map(|service| service.run.shutdown_timeout_seconds)
                .max()
                .unwrap_or(30),
            std::sync::atomic::Ordering::AcqRel,
        );
        self.shutdown_group_count.fetch_max(
            (enabled.count() as u64).saturating_add(1),
            std::sync::atomic::Ordering::AcqRel,
        );
        let rid = release.release_id.clone();
        *self
            .release
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(release);
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.release_id = Some(rid.clone());
        if let Some(op) = &mut status.operation
            && op.phase != AppCliDeployPhase::Failed
        {
            op.artifact_release_id = Some(rid);
        }
    }

    pub(super) fn shutdown_budget(&self, supervised: bool) -> std::time::Duration {
        let seconds = if supervised {
            // stop/remove are sequential RPCs (each bounded at 60 seconds),
            // bracketed by two process-group queries.
            self.shutdown_group_count
                .load(std::sync::atomic::Ordering::Acquire)
                .saturating_mul(2)
                .saturating_add(2)
                .saturating_mul(60)
        } else {
            // Parallel business grace + process tree force/reap confirmation.
            self.shutdown_grace_seconds
                .load(std::sync::atomic::Ordering::Acquire)
                .saturating_add(5)
        };
        // Remaining preparation/static writers and durable journal confirmation.
        std::time::Duration::from_secs(seconds.saturating_add(30))
    }

    /// 令牌回显登记（见 [`DeployStatus::request_release_id`]）：env 启动部署在进入
    /// Deploying 时调用；热部署路径在 [`Self::try_accept_deploy_with_id`] 受理时
    /// 已随 operation 同步登记。幂等重写，不随换代清除（标识最近一次请求）。
    pub(crate) fn set_request_release_id(&self, request_release_id: &str) {
        self.deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request_release_id = Some(request_release_id.to_owned());
    }

    /// 部署代（日志游标 boot_id 语义：换代后旧 cursor 失效重放）。
    pub(crate) fn boot_id(&self) -> String {
        self.release()
            .map(|r| r.release_id)
            .unwrap_or_else(|| "idle".to_string())
    }

    /// /ready 判定（api 探针 handler 消费）。
    pub(crate) fn readiness_ok(&self) -> bool {
        self.ready.is_ready() || self.phase().readiness_ok(false)
    }

    /// 热部署受理（api 端点调用）：相位守卫 + 通知主循环。
    #[cfg(test)]
    pub(crate) fn try_accept_deploy(&self, req: DeployRequest) -> Result<(), AdmissionError> {
        self.try_accept_deploy_with_id(req, uuid::Uuid::new_v4().simple().to_string())
            .map(|_| ())
    }

    pub(crate) fn try_accept_deploy_with_id(
        &self,
        req: DeployRequest,
        operation_id: String,
    ) -> Result<DeployAdmission, AdmissionError> {
        let _admission = self
            .admission
            .lock()
            .map_err(|_| "deployment admission lock poisoned")?;
        let fingerprint = deploy_replay::fingerprint(&req)?;
        let (old_receipt, mut history) = self.deployment_replay_snapshot()?;
        // Preserve old non-HTTP receipts too. Redacted credentials cannot be
        // used to reconstruct an input hash; only that old ID needs inspection.
        if let Some(receipt) = old_receipt.as_ref() {
            if !history.contains_key(&receipt.operation.operation_id) {
                let fingerprint = if receipt.request.run_pg.is_none() {
                    Some(deploy_replay::fingerprint(&receipt.request)?)
                } else {
                    None
                };
                history.insert(
                    receipt.operation.operation_id.clone(),
                    deploy_replay::Replay {
                        fingerprint,
                        operation: receipt.operation.clone(),
                    },
                );
            }
        } else if let Some(current) = self.deploy_status().operation
            && let Some(saved) = history.get_mut(&current.operation_id)
        {
            saved.operation = current;
        }
        // Replays are reads and precede busy/recovery checks: an uncertain
        // result is returned as recorded, never executed a second time.
        if let Some(saved) = history.get(&operation_id) {
            return deploy_replay::check(saved, &fingerprint).map(DeployAdmission::Replayed);
        }
        if let Some((operation, stage)) = self.native_control_blocker() {
            return Err(AdmissionError::Busy(format!(
                "native control operation {operation} is stopping business ({stage}); retry after it completes"
            )));
        }
        if !self.accepting.load(std::sync::atomic::Ordering::Acquire) {
            return Err(AdmissionError::Busy(
                "server is shutting down; deployment was not accepted".into(),
            ));
        }
        let supplying_credentials = self
            .can_supply_run_credentials(req.run_pg.as_ref(), false)
            .map_err(|error| {
                AdmissionError::Busy(format!("verify deployment credential recovery: {error:#}"))
            })?;
        // B05：恢复保护约束**所有**写入口——旧部署链不得绕过（启动序列已
        // 自动收敛未终态操作并隔离损坏记录；到达此门说明存储级故障仍在，
        // 旧链与显式部署同样拒绝，直至存储恢复并重启）。
        // R05：kernel 不可用（状态根打开失败）同样 fail-closed——旧链只在
        // Some(kernel) 上检查保护会把"可信状态不可读"当成"无保护可查"放行。
        match self.runtime_kernel() {
            Some(kernel) if kernel.recovery_protection_active() => {
                return Err(AdmissionError::Busy(
                    "runtime state requires recovery; resolve held operations before deploying"
                        .into(),
                ));
            }
            _ if self.kernel_unavailable() => {
                return Err(AdmissionError::Busy(
                    "runtime state unavailable (state root could not be opened); deployment \
                     admission closed until recovery"
                        .into(),
                ));
            }
            _ if self.runtime_recovery_hold_active() && !supplying_credentials => {
                // V04：运行操作终态持久化失败（结果未知）——身份保留中，
                // 新部署不得受理，直至重启恢复
                return Err(AdmissionError::Busy(
                    "runtime operation outcome could not be persisted; recovery required \
                     before deploying"
                        .into(),
                ));
            }
            _ => {}
        }
        let phase = self.phase();
        if !phase.accepts_deploy() {
            return Err(AdmissionError::Busy(format!(
                "deploy in progress (phase={}); retry after terminal",
                phase.as_str()
            )));
        }
        // Keep the shared admission lock throughout journal persistence and
        // dispatch. Only the exact credentials-only state can be consumed;
        // a concurrent uncertain writer must continue to block admission.
        if supplying_credentials {
            self.consume_credentials_hold(&operation_id)
                .map_err(|error| AdmissionError::Busy(format!("{error:#}")))?;
        }
        let mut credentials = CredentialAdmission {
            state: self,
            operation_id: supplying_credentials.then(|| operation_id.clone()),
        };
        let previous = self.deploy_status();
        let operation = shared_types::AppDeploymentOperation {
            operation_id,
            deployment_generation_id: self.generation_value(),
            deploy_stage: AppDeploymentStage::Pending,
            persisted: false,
            request_release_id: req.release_id.clone(),
            artifact_release_id: None,
            recovery: None,
            phase: AppCliDeployPhase::Deploying,
            error: None,
        };
        let mut journal_guard = self
            .journal
            .lock()
            .map_err(|_| "deployment journal lock poisoned")?;
        let mut volatile = self
            .volatile_deploy_replays
            .lock()
            .map_err(|_| "deployment replay lock poisoned")?;
        let old_history = journal_guard
            .as_ref()
            .map(|j| j.deploy_replays.clone())
            .unwrap_or_else(|| volatile.clone());
        history.insert(
            operation.operation_id.clone(),
            deploy_replay::Replay {
                fingerprint: Some(fingerprint),
                operation: operation.clone(),
            },
        );
        if let Some(journal) = journal_guard.as_mut() {
            journal
                .write_with_history(
                    Receipt {
                        generation: self.generation_value(),

                        operation: operation.clone(),
                        request: req.clone(),
                        boundary: Boundary::Preparing,
                        active: old_receipt
                            .as_ref()
                            .filter(|r| r.generation == self.generation_value())
                            .and_then(|r| r.active.clone())
                            .or_else(|| {
                                (phase == ServerPhase::Running)
                                    .then(|| self.release())
                                    .flatten()
                                    .map(|release| ActiveVersion {
                                        artifact_release_id: release.release_id,
                                        request: None,
                                    })
                            }),
                    },
                    history.clone(),
                )
                .map_err(|error| format!("persist deployment admission: {error:#}"))?;
        }
        let mut status = self
            .deploy_status
            .write()
            .map_err(|_| "deployment status lock poisoned")?;
        *status = DeployStatus {
            protocol_version: DEPLOY_PROTOCOL,
            operation: Some(operation),
            phase: AppCliDeployPhase::Deploying,
            release_id: previous.release_id.clone(),
            request_release_id: Some(req.release_id.clone()),
            error: None,
            capabilities: vec![
                "progress_v1".into(),
                "deployment_run_pg".into(),
                shared_types::BUSINESS_READINESS_CAPABILITY.into(),
            ],
            progress: None,
        };
        *self
            .phase
            .write()
            .map_err(|_| "deployment phase lock poisoned")? = ServerPhase::Deploying;
        if self.deploy_tx.send(req).is_err() {
            *self
                .phase
                .write()
                .map_err(|_| "deployment phase lock poisoned")? = phase;
            *status = previous;
            if let Some(journal) = journal_guard.as_mut() {
                match old_receipt {
                    Some(receipt) => journal.write_with_history(receipt, old_history),
                    None => journal.clear(),
                }
                .map_err(|e| format!("restore admission receipt: {e:#}"))?;
            }
            return Err("server loop exited".into());
        }
        if journal_guard.is_none() {
            *volatile = history;
        }
        credentials.operation_id = None;
        Ok(DeployAdmission::Accepted)
    }

    fn deployment_replay_snapshot(
        &self,
    ) -> Result<(Option<Receipt>, deploy_replay::History), AdmissionError> {
        let journal = self
            .journal
            .lock()
            .map_err(|_| "deployment journal lock poisoned")?;
        if let Some(journal) = journal.as_ref() {
            return Ok((journal.receipt.clone(), journal.deploy_replays.clone()));
        }
        let history = self
            .volatile_deploy_replays
            .lock()
            .map_err(|_| "deployment replay lock poisoned")?
            .clone();
        Ok((None, history))
    }

    pub(crate) fn recorded_deployment(
        &self,
        operation_id: &str,
    ) -> Result<Option<shared_types::AppDeploymentOperation>, AdmissionError> {
        let journal = self
            .journal
            .lock()
            .map_err(|_| "deployment journal lock poisoned")?;
        if let Some(journal) = journal.as_ref() {
            return Ok(journal
                .receipt
                .as_ref()
                .filter(|receipt| receipt.operation.operation_id == operation_id)
                .map(|receipt| receipt.operation.clone())
                .or_else(|| {
                    journal
                        .deploy_replays
                        .get(operation_id)
                        .map(|replay| replay.operation.clone())
                }));
        }
        if let Some(current) = self.deploy_status().operation
            && current.operation_id == operation_id
        {
            return Ok(Some(current));
        }
        Ok(self
            .volatile_deploy_replays
            .lock()
            .map_err(|_| "deployment replay lock poisoned")?
            .get(operation_id)
            .map(|replay| replay.operation.clone()))
    }

    /// Runtime admission already owns the operation slot. Persist its execution
    /// receipt before preparation, source switching, or any business mutation.
    pub(super) fn record_runtime_deployment(&self, request: &DeployRequest) -> Result<()> {
        let Some(operation_id) = request.runtime_operation_id.as_ref() else {
            return Ok(()); // legacy admission already wrote this receipt
        };
        let _admission = self
            .admission
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment admission lock poisoned"))?;
        let mut journal = self
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
        let Some(journal) = journal.as_mut() else {
            return Ok(());
        };
        if let Some(receipt) = &journal.receipt
            && receipt.operation.operation_id == *operation_id
        {
            anyhow::ensure!(
                receipt.request.execution_target == request.execution_target
                    && receipt.request.local_path == request.local_path
                    && receipt.request.release_id == request.release_id,
                "runtime execution receipt target changed for the same operation"
            );
            return Ok(());
        }
        let previous = self.deploy_status();
        let active = journal
            .receipt
            .as_ref()
            .filter(|receipt| receipt.generation == self.generation_value())
            .and_then(|receipt| receipt.active.clone())
            .or_else(|| {
                self.release().map(|release| ActiveVersion {
                    artifact_release_id: release.release_id,
                    request: None,
                })
            });
        let operation = shared_types::AppDeploymentOperation {
            operation_id: operation_id.clone(),
            deployment_generation_id: self.generation_value(),
            deploy_stage: AppDeploymentStage::Pending,
            persisted: false,
            request_release_id: request.release_id.clone(),
            artifact_release_id: None,
            recovery: None,
            phase: AppCliDeployPhase::Deploying,
            error: None,
        };
        journal.write(Receipt {
            generation: self.generation_value(),

            operation: operation.clone(),
            request: request.clone(),
            boundary: Boundary::Preparing,
            active,
        })?;
        *self
            .deploy_status
            .write()
            .map_err(|_| anyhow::anyhow!("deployment status lock poisoned"))? = DeployStatus {
            protocol_version: DEPLOY_PROTOCOL,
            operation: Some(operation),
            phase: AppCliDeployPhase::Deploying,
            release_id: previous.release_id,
            request_release_id: Some(request.release_id.clone()),
            error: None,
            capabilities: vec![
                "progress_v1".into(),
                "deployment_run_pg".into(),
                shared_types::BUSINESS_READINESS_CAPABILITY.into(),
            ],
            progress: None,
        };
        Ok(())
    }

    pub(crate) fn matches_generation(&self, generation: &str) -> bool {
        generation == self.generation_value()
    }

    pub(super) fn persist_boundary(&self, boundary: Boundary) -> Result<()> {
        let mut guard = self
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
        let Some(journal) = guard.as_mut() else {
            return Ok(());
        };
        let Some(mut receipt) = journal.receipt.clone() else {
            return Ok(());
        };
        let Some(operation) = self.deploy_status().operation else {
            // Existing code without an attached deployment attempt must never
            // promote a stale or foreign operation merely because it is healthy.
            return Ok(());
        };
        anyhow::ensure!(
            receipt.generation == self.generation_value()
                && operation.deployment_generation_id == self.generation_value()
                && receipt.operation.operation_id == operation.operation_id,
            "deployment receipt does not belong to the current operation"
        );
        if boundary == Boundary::RestoredActive {
            let active = receipt
                .active
                .as_ref()
                .context("restored artifact missing")?;
            anyhow::ensure!(
                matches!(
                    receipt.boundary,
                    Boundary::Preparing | Boundary::Switching | Boundary::RestoredActive
                ) && operation.phase == AppCliDeployPhase::Failed
                    && self
                        .release()
                        .is_some_and(|release| release.release_id == active.artifact_release_id),
                "restoration requires the confirmed previous artifact and failed preparation"
            );
        }
        receipt.boundary = boundary.clone();
        receipt.operation = operation;
        if matches!(boundary, Boundary::Activated | Boundary::Active) {
            receipt.operation.deploy_stage = AppDeploymentStage::Succeeded;
            receipt.operation.persisted = true;
        }
        if matches!(boundary, Boundary::Activated | Boundary::Active) {
            receipt.active = Some(ActiveVersion {
                request: Some(receipt.request.clone()),
                artifact_release_id: receipt
                    .operation
                    .artifact_release_id
                    .clone()
                    .context("activated operation has no artifact identity")?,
            });
            if boundary == Boundary::Active {
                receipt.operation.phase = AppCliDeployPhase::Running;
            }
        }
        journal.write(receipt)
    }

    pub(super) fn fail_operation(&self, error: String, boundary: Boundary) -> Result<()> {
        let _admission = self
            .admission
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment admission lock poisoned"))?;
        let mut snapshot = self.deploy_status();
        if let Some(operation) = snapshot.operation.as_ref() {
            self.settle_credential_recovery(&operation.operation_id, false);
        }
        snapshot.phase = AppCliDeployPhase::Failed;
        snapshot.error = Some(error.clone());
        if let Some(operation) = snapshot.operation.as_mut() {
            operation.phase = AppCliDeployPhase::Failed;
            operation.error = Some(error.clone());
            if operation.deploy_stage == AppDeploymentStage::Pending {
                operation.deploy_stage = AppDeploymentStage::Failed;
            }
            operation.persisted = true;
        }
        let mut guard = self
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
        if let Some(journal) = guard.as_mut()
            && let Some(mut receipt) = journal.receipt.clone()
            && let Some(operation) = snapshot.operation.as_ref()
        {
            anyhow::ensure!(
                receipt.generation == self.generation_value()
                    && operation.deployment_generation_id == self.generation_value()
                    && receipt.operation.operation_id == operation.operation_id,
                "failure receipt does not belong to the current operation"
            );
            if boundary == Boundary::StartupFailed {
                anyhow::ensure!(
                    matches!(
                        receipt.boundary,
                        Boundary::Activated | Boundary::Active | Boundary::StartupFailed
                    ),
                    "Startup failure requires a confirmed activation boundary"
                );
                receipt.active = Some(ActiveVersion {
                    request: Some(receipt.request.clone()),
                    artifact_release_id: receipt
                        .operation
                        .artifact_release_id
                        .clone()
                        .context("confirmed startup failure has no artifact identity")?,
                });
            }
            if boundary == Boundary::Failed {
                // Previous artifacts remain on disk, but are not a serving version
                // eligible for automatic restoration after activation has started.
                receipt.active = None;
            }
            receipt.boundary = boundary;
            if let Some(operation) = snapshot.operation.clone() {
                receipt.operation = operation;
            }
            journal.write(receipt)?;
        }
        *self
            .phase
            .write()
            .map_err(|_| anyhow::anyhow!("deployment phase lock poisoned"))? =
            ServerPhase::Failed(error);
        *self
            .deploy_status
            .write()
            .map_err(|_| anyhow::anyhow!("deployment status lock poisoned"))? = snapshot;
        Ok(())
    }

    pub(super) fn complete_stage(&self) -> Result<()> {
        self.persist_boundary(Boundary::Activated)?;
        let mut status = self
            .deploy_status
            .write()
            .map_err(|_| anyhow::anyhow!("deployment status lock poisoned"))?;
        if let Some(operation) = status.operation.as_mut() {
            operation.deploy_stage = AppDeploymentStage::Succeeded;
            operation.persisted = true;
        }
        Ok(())
    }

    pub(super) fn record_prepared_artifact(&self, artifact_release_id: String) -> Result<()> {
        anyhow::ensure!(
            !artifact_release_id.is_empty(),
            "prepared artifact identity is empty"
        );
        let mut status = self
            .deploy_status
            .write()
            .map_err(|_| anyhow::anyhow!("deployment status lock poisoned"))?;
        let operation = status
            .operation
            .as_mut()
            .context("prepared operation missing")?;
        anyhow::ensure!(
            operation.phase != AppCliDeployPhase::Failed,
            "cannot prepare an already failed operation"
        );
        // Do not replace self.release or journal.active before activation.
        operation.artifact_release_id = Some(artifact_release_id);
        Ok(())
    }

    pub(super) fn complete_running(&self) -> Result<()> {
        // An earlier prepare failure is still the result of that attempt; merely
        // restoring service health cannot turn it into a successful deployment.
        let failed = self
            .deploy_status()
            .operation
            .as_ref()
            .is_some_and(|op| op.phase == AppCliDeployPhase::Failed);
        if !failed {
            self.persist_boundary(Boundary::Active)?;
        } else {
            self.persist_boundary(Boundary::RestoredActive)?;
        }
        if !failed && let Some(operation) = self.deploy_status().operation {
            self.settle_credential_recovery(&operation.operation_id, true);
        }
        self.set_phase(ServerPhase::Running);
        Ok(())
    }

    pub(crate) fn deploy_status(&self) -> DeployStatus {
        self.deploy_status
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// RuntimeStatusService 句柄（编排器 set_ready / api /ready 消费共享）。
    pub(crate) fn runtime_status(&self) -> RuntimeStatusService {
        self.ready.clone()
    }

    pub(crate) fn log_layout(&self) -> LogLayout {
        *self
            .log_layout
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn set_log_layout(&self, layout: LogLayout) {
        *self
            .log_layout
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = layout;
    }
}
