//! 主清理器
//!
//! 协调各模块完成清理任务

use anyhow::{Context, Result};
use chrono::Utc;
// 存储契约 trait：state.projects（ProjectStoreBackend 枚举）上的方法经此解析
use shared_types::ProjectStore as _;
use shared_types::ServiceType;
use std::sync::Arc;
use tokio::time::interval;
use tracing::{debug, info, warn};

mod idle_sweep;
use idle_sweep::{IdleConfirmation, IdleSweep, IdleWorkload};

/// 主清理器（协调各模块）
pub struct AgentCleaner {
    config: super::config::CleanupConfig,
    stats: super::config::CleanupStats,
    state: Arc<crate::app_state::AppState>,

    // 策略
    rcoder_strategy: super::strategies::rcoder::RCoderStrategy,
    computer_runner_strategy: super::strategies::computer_runner::ComputerRunnerStrategy,

    // 组件
    container_destroyer: super::container::ContainerDestroyer,
    agent_scanner: super::agent::AgentScanner,
    log_cleaner: super::logs::LogCleaner,

    /// Qualified idle observations from the last completed sweep only.
    /// Shared projects cannot advance a physical workload twice in one sweep.
    pending_destroy: std::collections::HashSet<IdleWorkload>,
}

impl AgentCleaner {
    pub fn new(
        config: super::config::CleanupConfig,
        state: Arc<crate::app_state::AppState>,
        pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    ) -> Self {
        let config_clone = config.clone();
        let state_clone = state.clone();
        let grpc_pool = state.grpc_pool.clone();
        let namespace = state.config.app_manager.namespace.clone();
        // 运行时抽象 (Docker / K8s) —— destroyer 物理销毁走它, 使 AgentCleaner 跨模式复用
        let runtime = state.runtime().clone();

        // 判断是否是 K8s 运行时（通过 features flag）
        let is_kubernetes = shared_types::is_kubernetes_runtime();

        // 创建日志清理器（使用配置）
        let log_cleaner = super::logs::LogCleaner::new(
            config.log_dir.clone(),
            config.log_retention_duration.as_secs() / 24 / 60 / 60,
        );

        Self {
            config,
            stats: super::config::CleanupStats::default(),
            state: state_clone,
            rcoder_strategy: super::strategies::rcoder::RCoderStrategy,
            computer_runner_strategy: super::strategies::computer_runner::ComputerRunnerStrategy,
            container_destroyer: super::container::ContainerDestroyer::new(
                runtime,
                grpc_pool,
                pingora_service,
                namespace,
                state.cluster_domain.clone(),
                is_kubernetes,
            ),
            agent_scanner: {
                use crate::cleanup_task::agent::AgentScanner;
                AgentScanner::new(state.clone(), config_clone)
            },
            log_cleaner,
            pending_destroy: std::collections::HashSet::new(),
        }
    }

    /// 执行一次清理
    pub async fn cleanup_once(&mut self) -> Result<super::config::CleanupStats> {
        let start_time = std::time::Instant::now();
        // A failed/cancelled sweep breaks continuity. Only a completed sweep
        // publishes its qualified observations for the next independent scan.
        let mut idle_sweep = IdleSweep::new(std::mem::take(&mut self.pending_destroy));

        // 重置本次清理的统计
        let mut current_stats = super::config::CleanupStats::default();

        // 1. 清理过期日志文件
        match self.log_cleaner.cleanup_once().await {
            Ok(log_stats) => {
                if log_stats.files_deleted > 0 || log_stats.failed_deletions > 0 {
                    info!("[cleaner] Cleanup completed: {}", log_stats.summary());
                }
            }
            Err(e) => {
                warn!("[cleaner] Cleanup failed: {}", e);
            }
        }

        // 2. 扫描需要清理的 agent
        let idle_agents = self.agent_scanner.scan_idle_agents().await?;
        info!("[cleaner] Found {} idle agents to clean", idle_agents.len());

        // 3. 清理每个 agent
        for project_id in idle_agents {
            // Retiring a captured workload can remove all its old references.
            // Never erase a surviving/new registration just because a sibling
            // from the scanner's snapshot has already been retired.
            if !self.state.contains_project(&project_id) {
                info!(
                    "[cleaner] Project already removed (likely by shared container cleanup): project_id={}",
                    project_id
                );
                current_stats.total_cleaned += 1;
                current_stats.success_cleaned += 1;
                continue;
            }

            current_stats.total_cleaned += 1;

            match self.cleanup_agent(&project_id, &mut idle_sweep).await {
                Ok(destroyed) => {
                    current_stats.success_cleaned += 1;
                    if destroyed {
                        current_stats.containers_destroyed += 1;
                    }
                    info!("[cleaner] Agent cleanup succeeded: {}", project_id);
                }
                Err(e) => {
                    current_stats.failed_cleaned += 1;
                    warn!("[cleaner] Agent cleanup failed: {} - {e:#}", project_id);
                }
            }
        }
        self.pending_destroy = idle_sweep.into_pending();

        // 4. 更新累计统计
        current_stats.last_cleanup = Some(Utc::now());
        self.stats.total_cleaned += current_stats.total_cleaned;
        self.stats.success_cleaned += current_stats.success_cleaned;
        self.stats.failed_cleaned += current_stats.failed_cleaned;
        self.stats.containers_destroyed += current_stats.containers_destroyed;
        self.stats.last_cleanup = current_stats.last_cleanup;

        let duration = start_time.elapsed();
        info!(
            "[cleaner] Cleanup completed, duration: {:.2}s, this run: {}",
            duration.as_secs_f64(),
            current_stats.summary()
        );
        info!("[cleaner] Stats: {}", self.stats.summary());

        Ok(current_stats)
    }

    /// 清理单个 agent
    /// Ok(true) confirms compute retirement; Ok(false) retains/skips it.
    async fn cleanup_agent(&self, project_id: &str, idle_sweep: &mut IdleSweep) -> Result<bool> {
        info!("[cleaner] Starting cleanup agent: {}", project_id);

        // 1. 获取项目信息
        let agent_info = self
            .state
            .get_project(project_id)
            .ok_or_else(|| anyhow::anyhow!("Agent does not exist: {}", project_id))?;
        let captured_container = agent_info.container_info();
        if let Some(container) = captured_container.as_ref()
            && !idle_sweep.observe_once(container)
        {
            debug!(project_id, container_id = %container.container_id,
                "[cleaner] Physical workload already observed in this sweep");
            return Ok(false);
        }

        // 🛡️ 竞态保护：scanner 扫描到 cleaner 处理之间有时间差（gRPC 二次确认等），
        // 期间 project 可能收到 keepalive/chat 刷新了 last_activity。
        // 重新校验 idle_duration，避免误杀刚被保活的项目。
        // （共享容器场景额外由 strategy 的 has_active_refs 兜底，这里覆盖独立容器场景）
        let idle_secs = (Utc::now() - agent_info.last_activity()).num_seconds();
        if idle_secs < self.config.idle_timeout.as_secs() as i64 {
            info!(
                "[cleaner] Project activity refreshed after scan, skip cleanup: project_id={}, idle_secs={}s",
                project_id, idle_secs
            );
            return Ok(false);
        }

        let service_type = agent_info
            .service_type()
            .unwrap_or(ServiceType::WebAgentRunner);

        // 🛡️ scope 围栏：builder 闲置回收不经过生命周期受理，必须显式尊重
        // 在途 dev/application 操作（Ensure/Restart/StopBuilder、Destroy/ClearDevStorage、
        // DeleteApplication/PurgeResources）。读失败同样跳过——未知状态不是销毁许可。
        if service_type == ServiceType::UserappBuilder
            && builder_fenced_by_active_operations(self.state.userapp_store.as_ref(), project_id)
                .await
        {
            info!(
                "[cleaner] 🛡️ builder fenced by active userapp operation, skip reaping this round: app_id={}",
                project_id
            );
            return Ok(false);
        }

        // 2. 选择策略
        let strategy: &dyn super::strategies::CleanupStrategy = match service_type {
            ServiceType::WebAgentRunner => &self.rcoder_strategy,
            // Computer 族共享 per-user 容器：常规项目同用 computer 策略（按 user 引用计数）
            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
                &self.computer_runner_strategy
            }
            // Userapp 不注册到 projects 表,cleanup_task 不会处理;UserappBuilder 复用 agent-runner,
            // 闲置清理用 rcoder 策略(检查自身 idle + timeout)。兜底用 rcoder 策略。
            ServiceType::Userapp | ServiceType::UserappBuilder => &self.rcoder_strategy,
        };

        // 3. 检查是否需要销毁容器，并获取销毁原因
        let context = super::strategies::CleanupContext {
            state: self.state.clone(),
            config: self.config.clone(),
        };

        let destroy_reason = strategy
            .should_destroy_container(project_id, &context)
            .await?;

        // 短期闲置（idle > idle_timeout 但 < long_idle_timeout）：标记 Idle、保留容器 + project 复用，
        // 不销毁、不删 project。只有长期闲置（>= long_idle_timeout）才走下方二次确认 + 销毁链。
        //
        // 🔄 滚动升级放行：镜像漂移（rcoder 已升版而 agent 仍旧版——STS 模板仅创建时
        // 固化）时跳过"标记保留"，直接落入下方二次确认 + 销毁链——换代阈值从
        // long_idle(1h) 收紧到 idle_timeout(10min)。本分支仅在 idle > idle_timeout 时
        // 到达（活跃会话不受影响），且仍享二次确认闸门保护。
        let rollout_ready = destroy_reason.is_some()
            && idle_secs < self.config.long_idle_timeout.as_secs() as i64
            && self.check_image_drift(&agent_info, &service_type).await;
        if destroy_reason.is_some()
            && idle_secs < self.config.long_idle_timeout.as_secs() as i64
            && !rollout_ready
        {
            self.state
                .projects
                .update_agent_status(project_id, 0, "idle");
            info!(
                "[cleaner] 📌 idle 标记(保留复用): project_id={}, idle_secs={}s < long_idle={}s",
                project_id,
                idle_secs,
                self.config.long_idle_timeout.as_secs()
            );
            return Ok(false); // 不销毁容器、不删 project
        }

        // Confirm only observations published by the previous completed sweep.
        // Current activity, canonical references and exact runtime identity are
        // still rechecked below before any physical stop.
        if destroy_reason.is_some()
            && let Some(container) = captured_container.as_ref()
        {
            match idle_sweep.confirm(container) {
                IdleConfirmation::Pending => {
                    info!(project_id, container_id = %container.container_id,
                        "[cleaner] idle first seen; reconfirm in a later cleanup sweep");
                    return Ok(false);
                }
                IdleConfirmation::Confirmed => {
                    info!(project_id, container_id = %container.container_id,
                        "[cleaner] idle confirmed in distinct consecutive cleanup sweeps");
                }
            }
        }

        if destroy_reason.is_none() {
            return Ok(false);
        }
        // A strategy declining destruction is not permission to erase a
        // current registration. Retain it for shared-workload reuse/read APIs.
        let (Some(reason), Some(container_info)) = (destroy_reason, captured_container.as_ref())
        else {
            return Ok(false);
        };
        let identifier = service_type
            .container_identifier(agent_info.pod_id(), agent_info.user_id(), Some(project_id))?
            .to_owned();
        let admission = self
            .state
            .acquire_agent_workload(
                &identifier,
                &service_type,
                rcoder_storage::WorkloadAdmissionMode::Exclusive,
            )
            .await?;
        // Builder retains its existing lifecycle/operation admission. The
        // additional catalog guard protects the registration CAS, not a bypass.
        // Keep the creation order: workload admission precedes local lifecycle.
        let _builder_retirement = if service_type == ServiceType::UserappBuilder {
            let Some(guard) = crate::userapp_builder::try_idle_builder_retirement(project_id).await
            else {
                return Ok(false);
            };
            Some(guard)
        } else {
            None
        };
        let frame = self
            .state
            .projects
            .capture_workload_registration(&admission, admission.key().logical_name())
            .await?;
        if !frame.project_ids().contains(&project_id) {
            // A stale local mirror cannot turn a registry-absent live Pod into
            // a registered idle deletion candidate. Orphan GC observes it.
            return Ok(false);
        }
        // Read every canonical reference, including rows not yet in this
        // replica's mirror. Unknown or recently refreshed activity skips this
        // round; it is not evidence that compute has become idle.
        for id in frame.project_ids() {
            let Some(current) = self.state.projects.refresh_project_registration(id).await? else {
                return Ok(false);
            };
            if (Utc::now() - current.last_activity()).num_seconds()
                < self.config.idle_timeout.as_secs() as i64
                || current.container_info().as_ref().is_none_or(|actual| {
                    actual.container_id != container_info.container_id
                        || actual.workload_uid != container_info.workload_uid
                })
            {
                return Ok(false);
            }
        }
        let address = match crate::utils::extract_grpc_addr_with_port(
            &container_info.service_url,
            shared_types::GRPC_DEFAULT_PORT,
        ) {
            Ok(address) => address,
            Err(error) => {
                warn!(project_id, %error, "skip idle retirement: activity endpoint is unreadable");
                return Ok(false);
            }
        };
        // Capture before the final live observation. A same-name successor
        // appearing during that RPC can never become this cleanup's target.
        let runtime_snapshot = if service_type == ServiceType::UserappBuilder {
            None
        } else {
            let snapshot = self
                .state
                .runtime()
                .capture_agent_stop(&identifier, &service_type)
                .await?;
            let observed = snapshot
                .container
                .as_ref()
                .or_else(|| snapshot.pods.first());
            if observed.is_none_or(|observed| observed.uid != container_info.container_id) {
                return Ok(false);
            }
            Some(snapshot)
        };
        match self
            .agent_scanner
            .status_checker
            .is_container_active(
                &address,
                agent_info.user_id().unwrap_or(project_id),
                project_id,
            )
            .await
        {
            Ok(false) => {}
            Ok(true) => return Ok(false),
            Err(error) => {
                warn!(project_id, %error, "skip idle retirement: current execution is unknown");
                return Ok(false);
            }
        }
        admission.verify_current().await?;
        info!(project_id, identifier = %identifier, container_name = %container_info.container_name,
            container_id = %container_info.container_id, reason = reason.as_str(), reason_detail = %reason.description(),
            "Retiring idle compute after current activity and canonical reference checks");
        if service_type == ServiceType::UserappBuilder {
            if builder_fenced_by_active_operations(self.state.userapp_store.as_ref(), project_id)
                .await
            {
                return Ok(false);
            }
            self.container_destroyer
                .destroy_idle_builder_compute(&identifier, container_info, &reason)
                .await?;
            let outcome = self
                .state
                .retire_stopped_agent_registration(&admission, &frame)
                .await?;
            anyhow::ensure!(
                outcome == shared_types::persistence::PersistenceWriteOutcome::Committed,
                "Builder stopped but registration retirement is not confirmed: {outcome:?}"
            );
            self.state.grpc_pool.remove(&address).await;
        } else if let Some(snapshot) = runtime_snapshot.as_ref() {
            self.state
                .stop_captured_ordinary_agent_workload(&admission, snapshot)
                .await
                .with_context(|| format!("Idle compute retirement: project_id={project_id}, identifier={identifier}, container_id={}, reason={}",
                    container_info.container_id, reason.description()))?;
        }
        info!(project_id, container_id = %container_info.container_id, reason = reason.as_str(), reason_detail = %reason.description(),
            "Idle compute and its captured canonical registration retired");
        Ok(true)
    }

    /// 滚动升级判据：agent 容器镜像是否落后于当前进程期望。
    ///
    /// 仅对短期闲置候选调用（活跃 agent 与长期闲置均不查——后者本来就会销毁
    /// 换代），控制 K8s API 量。查询失败按不漂移处理（换代不因 API 抖动误触发）；
    /// Docker 模式 runtime 恒返回 false（容器重建即新镜像）。
    async fn check_image_drift(
        &self,
        info: &Arc<shared_types::ProjectAndContainerInfo>,
        service_type: &ServiceType,
    ) -> bool {
        if info.container_info().is_none() {
            return false; // 无容器记录：无换代对象
        }
        // identifier 单一事实源（与**创建路径**一致：pod_id 优先 →
        // Computer 用 user_id → Web/Userapp 用 project_id），防止此处的 STS 定位
        // 与创建路径分叉
        let identifier = match service_type.container_identifier(
            info.pod_id(),
            info.user_id(),
            Some(info.project_id()),
        ) {
            Ok(id) => id,
            Err(e) => {
                debug!("[cleaner] rollout check skipped (no identifier): {e}");
                return false;
            }
        };
        match self
            .state
            .runtime()
            .is_agent_image_drifted(identifier, service_type)
            .await
        {
            Ok(true) => {
                info!(
                    "[cleaner] 🔄 agent image drifted, will roll on idle: project_id={}, identifier={}",
                    info.project_id(),
                    identifier
                );
                true
            }
            Ok(false) => false,
            Err(e) => {
                warn!(
                    "[cleaner] image drift check failed (skip rollout this round): {}",
                    e
                );
                false
            }
        }
    }

    /// 运行清理任务（定时）。收到 shutdown 信号后优雅退出。
    pub async fn run(&mut self, mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) {
        info!("[cleaner] cleanup task already started");

        let mut interval = interval(self.config.cleanup_interval);
        let mut memory_log_counter = 0u32;
        const MEMORY_LOG_INTERVAL: u32 = 12; // 每 12 次清理（大约 1 小时）输出一次内存日志

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match self.cleanup_once().await {
                        Ok(_) => debug!("[cleaner] Cleanup completed"),
                        Err(e) => warn!("[cleaner] Cleanup failed: {}", e),
                    }

                    // 定期输出存储统计信息
                    memory_log_counter += 1;
                    if memory_log_counter >= MEMORY_LOG_INTERVAL {
                        memory_log_counter = 0;
                        let stats = self.state.projects.get_stats();
                        debug!(
                            "[cleaner] Storage Stats: projects={}, containers={}, sessions={}",
                            stats.total_projects, stats.total_containers, stats.active_sessions
                        );
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("[cleaner] shutdown");
                    break;
                }
            }
        }
    }
}

/// builder 闲置回收围栏：dev/application 槽位任一在途即不得回收。
/// 生命周期读取失败按围栏处理——未知操作状态不是销毁许可（fail-closed）。
pub(crate) async fn builder_fenced_by_active_operations(
    store: &dyn shared_types::UserAppLifecycleStore,
    app_id: &str,
) -> bool {
    match store.get_application(app_id).await {
        Ok(Some(app)) => {
            app.active_operations.dev.is_some() || app.active_operations.application.is_some()
        }
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(
                %app_id, %error,
                "[cleaner] builder lifecycle read failed; treating as fenced"
            );
            true
        }
    }
}

#[cfg(test)]
mod idle_tests;

#[cfg(test)]
mod scope_fence_tests {
    use super::builder_fenced_by_active_operations;
    use shared_types::{
        UserAppAdmission, UserAppAdmissionOutcome, UserAppLifecycleStore as _, UserAppOperationKind,
    };

    fn request(app_id: &str, kind: UserAppOperationKind) -> UserAppAdmission {
        UserAppAdmission {
            runtime_policy_on_success: None,
            command: None,
            metadata: None,
            app_id: app_id.into(),
            lifecycle_id: None,
            operation_id: format!("fence-{app_id}-{}", kind_name(kind)),
            request_id: Some(format!("fence-{app_id}")),
            request_fingerprint: "a".repeat(64),
            kind,
        }
    }

    fn kind_name(kind: UserAppOperationKind) -> &'static str {
        match kind {
            UserAppOperationKind::RestartBuilder => "restart",
            UserAppOperationKind::Start => "start",
            UserAppOperationKind::DeleteApplication => "delete",
            _ => "other",
        }
    }

    #[tokio::test]
    async fn builder_reaping_respects_dev_and_application_scope_fences() {
        let directory = tempfile::tempdir().expect("directory");
        let store = rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("fence.turso.db"),
        )
        .await
        .expect("store");

        // 无生命周期记录：不围栏
        assert!(!builder_fenced_by_active_operations(&store, "fence-unknown").await);

        // 仅有 prod 在途操作：builder 回收不受 prod 域阻塞
        store.ensure_identity("fence-prod-only").await.unwrap();
        store
            .admit(&request("fence-prod-only", UserAppOperationKind::Start))
            .await
            .unwrap();
        assert!(!builder_fenced_by_active_operations(&store, "fence-prod-only").await);

        // dev 在途操作：围栏（cleaner 须跳过本轮）
        store.ensure_identity("fence-dev").await.unwrap();
        store
            .admit(&request("fence-dev", UserAppOperationKind::RestartBuilder))
            .await
            .unwrap();
        assert!(builder_fenced_by_active_operations(&store, "fence-dev").await);

        // application 在途操作：围栏（整体删除优先于回收）
        store.ensure_identity("fence-whole").await.unwrap();
        let outcome = store
            .admit(&request(
                "fence-whole",
                UserAppOperationKind::DeleteApplication,
            ))
            .await
            .unwrap();
        assert!(matches!(outcome, UserAppAdmissionOutcome::Accepted(_)));
        assert!(builder_fenced_by_active_operations(&store, "fence-whole").await);

        rcoder_storage::userapp_lifecycle::TursoUserAppStore::shutdown(&store)
            .await
            .expect("shutdown");
    }
}
