//! 协调器服务对象：`PreviewCoordination` trait 的实现。
//!
//! 职责边界（不变量来源 spec.md）：
//! - 受理/发布/停止全部经权威存储 CAS；执行只发生在宿主（本机执行器或经
//!   `RemoteDispatch` 到宿主 Pod 的内部端点），目标地址来自权威库行；
//! - keep-alive 存活判定=ready+新鲜心跳或宿主 verify；死实例统一受理重建，
//!   绝不在非宿主副本无条件重建；降级信封 HTTP 200 + success:false；
//! - activity 只进内存累积器（30s 批量刷盘，GREATEST 不回退）；
//! - Unknown 恢复必须凭宿主 Pod 不存在证据，不凭 TTL。
use std::sync::Arc;
use std::time::Duration;

use shared_types::preview::degraded_reason;
use shared_types::{
    AcceptStartInput, AcceptStartOutcome, ExecutorLogChunk, ExecutorStartTicket,
    ExecutorStopOutcome, ExecutorVerifyReport, PREVIEW_PORT_MAX, PREVIEW_PORT_MIN,
    PREVIEW_PORT_RESERVED_MAX, PREVIEW_PORT_RESERVED_MIN, PreviewCoordination,
    PreviewCoordinationError, PreviewForwardCheck, PreviewHostIdentity, PreviewInstanceRecord,
    PreviewInstanceState, PreviewKeepAliveEnvelope, PreviewKeepAliveRequest, PreviewKeyInput,
    PreviewListEntry, PreviewPortAllocation, PreviewPortPoolStatus, PreviewProjectIdentity,
    PreviewRestartEnvelope, PreviewRestartRequest, PreviewRouteResolution, PreviewStartEnvelope,
    PreviewStartRequest, PreviewStopEnvelope, PreviewStopRequest, PreviewStoreError,
    is_preview_port, preview_key,
};

use crate::activity::ActivityAccumulator;
use crate::cache::RouteCache;
use crate::config::CoordinatorConfig;
use crate::dispatch::RemoteDispatch;
use crate::evidence::{HostEvidence, pod_uid_of};
use crate::identity::local_host_identity;

const START_PORT_ATTEMPTS: usize = 3;

fn unavailable(context: impl std::fmt::Display) -> PreviewCoordinationError {
    PreviewCoordinationError::Unavailable(context.to_string())
}

fn conflict(context: impl std::fmt::Display) -> PreviewCoordinationError {
    PreviewCoordinationError::Conflict(context.to_string())
}

fn invalid(context: impl std::fmt::Display) -> PreviewCoordinationError {
    PreviewCoordinationError::Invalid(context.to_string())
}

fn store_error(error: PreviewStoreError) -> PreviewCoordinationError {
    match error {
        PreviewStoreError::Unavailable(detail) => unavailable(format!("preview store: {detail}")),
        PreviewStoreError::Conflict(detail) => conflict(detail),
        PreviewStoreError::Invalid(detail) => invalid(detail),
    }
}

/// base path 规范化（与 file-server `process::normalize_base_path` 同规则；
/// 协调器不依赖 file-server，故此处同源复制并以此注释锚定）。
fn normalize_base_path(base: &str) -> String {
    let b = base.trim();
    if b.is_empty() {
        return "/".to_string();
    }
    if let Some(stripped) = b.strip_prefix('/') {
        let s = stripped.trim_end_matches('/');
        if s.is_empty() {
            return "/".to_string();
        }
        return format!("/{s}/");
    }
    format!("/{}/", b.trim_end_matches('/'))
}

/// 默认 base（端口组合；与 build_dev_args 默认一致）。
fn default_base(port: u16) -> String {
    format!("/proxy/{port}/")
}

fn compute_key(identity: &PreviewProjectIdentity) -> String {
    preview_key(&PreviewKeyInput {
        project_id: &identity.project_id,
        tenant_id: identity.tenant_id.as_deref(),
        space_id: identity.space_id.as_deref(),
        isolation_type: identity.isolation_type.as_deref(),
        resolved_path: &identity.resolved_path,
    })
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub struct PreviewCoordinator {
    pub(crate) store: Arc<dyn shared_types::PreviewLifecycleStore>,
    pub(crate) executor: Arc<dyn shared_types::PreviewExecutor>,
    evidence: Arc<dyn HostEvidence>,
    dispatch: RemoteDispatch,
    /// 内部令牌（派发客户端与内部端点 guard 同源；显式持有避免 env 名漂移）。
    pub(crate) token: String,
    host: PreviewHostIdentity,
    config: CoordinatorConfig,
    route_cache: RouteCache,
    activity: ActivityAccumulator,
}

impl PreviewCoordinator {
    pub fn new(
        store: Arc<dyn shared_types::PreviewLifecycleStore>,
        executor: Arc<dyn shared_types::PreviewExecutor>,
        evidence: Arc<dyn HostEvidence>,
        internal_token: String,
        config: CoordinatorConfig,
    ) -> Self {
        let host = local_host_identity();
        let dispatch = RemoteDispatch::new(
            internal_token.clone(),
            config.peer_api_port,
            config.remote_dispatch_timeout_secs,
        );
        let route_cache = RouteCache::new(
            config.route_cache_positive_secs,
            config.route_cache_negative_secs,
        );
        Self {
            store,
            executor,
            evidence,
            dispatch,
            token: internal_token,
            host,
            config,
            route_cache,
            activity: ActivityAccumulator::default(),
        }
    }

    pub fn host(&self) -> &PreviewHostIdentity {
        &self.host
    }

    pub fn route_cache(&self) -> &RouteCache {
        &self.route_cache
    }

    pub fn activity(&self) -> &ActivityAccumulator {
        &self.activity
    }

    pub fn config(&self) -> &CoordinatorConfig {
        &self.config
    }

    /// 内部令牌（rcoder 装配 Pingora 转发槽回填用；与派发客户端/内部端点同源）。
    pub fn internal_token(&self) -> &str {
        &self.token
    }

    fn heartbeat_fresh(&self, row: &PreviewInstanceRecord) -> bool {
        row.last_heartbeat_at.is_some_and(|at| {
            let age = chrono::Utc::now()
                .signed_duration_since(at)
                .to_std()
                .unwrap_or_default();
            age < Duration::from_secs(self.config.heartbeat_ttl_secs)
        })
    }

    /// Unknown 行恢复证据：宿主 Pod 不存在（K8s 查证/单机恒成立）。
    async fn recovery_evidence(&self, row: &PreviewInstanceRecord) -> Option<String> {
        let pod_uid = pod_uid_of(&row.host_id);
        if self.host_pod_exists(pod_uid).await {
            tracing::warn!(
                preview_key = %row.preview_key,
                host_id = %row.host_id,
                "unknown instance host pod still exists; refuse takeover"
            );
            return None;
        }
        Some(format!(
            "host pod {pod_uid} no longer exists (verified at startup/recovery)"
        ))
    }

    pub(crate) async fn host_pod_exists(&self, pod_uid: &str) -> bool {
        self.evidence.host_pod_exists(pod_uid).await
    }

    /// With positive evidence that the recorded host no longer exists, fence
    /// the exact instance through Unknown and settle it as Stopped. Both writes
    /// remain generation-bound in the store, so a replacement instance wins a
    /// concurrent race instead of being overwritten by this recovery.
    pub(crate) async fn settle_orphaned_instance(
        &self,
        row: &PreviewInstanceRecord,
        evidence: &str,
    ) -> Result<PreviewInstanceRecord, PreviewCoordinationError> {
        if evidence.is_empty() {
            return Err(invalid("orphaned preview recovery evidence is empty"));
        }
        let unknown = if row.state == PreviewInstanceState::Unknown {
            row.clone()
        } else {
            self.store
                .mark_unknown(&row.preview_key, &row.instance_id, evidence)
                .await
                .map_err(store_error)?
        };
        let stopped = self
            .store
            .resolve_unknown_stopped(&unknown.preview_key, &unknown.instance_id, evidence)
            .await
            .map_err(store_error)?;
        if let Some(port) = stopped.port {
            self.route_cache.invalidate(port);
        }
        tracing::info!(
            preview_key = %stopped.preview_key,
            instance_id = %stopped.instance_id,
            host_id = %stopped.host_id,
            "orphaned preview instance reconciled as stopped"
        );
        Ok(stopped)
    }

    async fn settle_if_host_absent(
        &self,
        row: &PreviewInstanceRecord,
    ) -> Result<bool, PreviewCoordinationError> {
        if row.host_id == self.host.host_id {
            return Ok(false);
        }
        let Some(evidence) = self.recovery_evidence(row).await else {
            return Ok(false);
        };
        self.settle_orphaned_instance(row, &evidence).await?;
        Ok(true)
    }

    fn start_envelope(row: &PreviewInstanceRecord, message: &str) -> PreviewStartEnvelope {
        PreviewStartEnvelope {
            success: true,
            message: message.to_string(),
            project_id: row.project_id.clone(),
            pid: row.pid.unwrap_or_default(),
            port: row.port.unwrap_or_default(),
        }
    }

    /// 受理 + 执行启动（端口有界重试）。
    async fn admit_and_start(
        &self,
        identity: &PreviewProjectIdentity,
        base_path: Option<String>,
    ) -> Result<PreviewStartEnvelope, PreviewCoordinationError> {
        let key = compute_key(identity);
        // 预读：幂等快路径 + Unknown 证据准备
        let mut recovery = None;
        if let Ok(Some(row)) = self.store.get(&key).await {
            match row.state {
                PreviewInstanceState::Ready if self.heartbeat_fresh(&row) => {
                    return Ok(Self::start_envelope(&row, "Development server started"));
                }
                PreviewInstanceState::Ready
                | PreviewInstanceState::Starting
                | PreviewInstanceState::Stopping
                    if self.settle_if_host_absent(&row).await? => {}
                PreviewInstanceState::Unknown => match self.recovery_evidence(&row).await {
                    Some(evidence) => {
                        recovery = Some(shared_types::PreviewRecoveryEvidence {
                            instance_id: row.instance_id.clone(),
                            revision: row.revision,
                            detail: evidence,
                        })
                    }
                    None => {
                        return Err(conflict(
                            "preview instance state unknown and host pod still exists; takeover refused",
                        ));
                    }
                },
                _ => {}
            }
        }

        let mut candidate: Option<u16> = None;
        for _ in 0..START_PORT_ATTEMPTS {
            let input = AcceptStartInput {
                preview_key: key.clone(),
                project_id: identity.project_id.clone(),
                project_path: identity.resolved_path.clone(),
                host: self.host.clone(),
                operation_id: new_id(),
                instance_id: new_id(),
                requested_port: candidate,
                recover_unknown_evidence: recovery.clone(),
            };
            match self.store.accept_start(input).await {
                Ok(AcceptStartOutcome::Admitted(record)) => {
                    match self
                        .execute_start(&key, base_path.as_deref(), &record)
                        .await
                    {
                        Ok(envelope) => return Ok(envelope),
                        Err(StartExecError::PortInUse(port)) => {
                            note_store_result(
                                self.store
                                    .mark_failed(
                                        &key,
                                        &record.instance_id,
                                        record.revision,
                                        &format!("port {port} occupied locally, retrying"),
                                    )
                                    .await,
                            );
                            candidate = Some(Self::next_candidate(port));
                            continue;
                        }
                        Err(StartExecError::Fatal(error)) => {
                            note_store_result(
                                self.store
                                    .mark_failed(
                                        &key,
                                        &record.instance_id,
                                        record.revision,
                                        &error.to_string(),
                                    )
                                    .await,
                            );
                            return Err(error);
                        }
                    }
                }
                Ok(AcceptStartOutcome::ExistingReady(row)) => {
                    return Ok(Self::start_envelope(&row, "Development server started"));
                }
                Ok(AcceptStartOutcome::Blocked(row)) => {
                    return Err(conflict(format!(
                        "preview operation in progress: state={:?}",
                        row.state
                    )));
                }
                Err(error) => return Err(store_error(error)),
            }
        }
        Err(unavailable("preview start exhausted port retry budget"))
    }

    /// 受理后已知的换端口候选（避开冲突口与保留区；分配器已排除全局占用）。
    fn next_candidate(prev: u16) -> u16 {
        let mut next = if prev >= PREVIEW_PORT_MAX {
            PREVIEW_PORT_MIN
        } else {
            prev + 1
        };
        if (PREVIEW_PORT_RESERVED_MIN..=PREVIEW_PORT_RESERVED_MAX).contains(&next) {
            next = PREVIEW_PORT_RESERVED_MAX + 1;
        }
        next
    }

    async fn execute_start(
        &self,
        key: &str,
        base_path: Option<&str>,
        record: &PreviewInstanceRecord,
    ) -> Result<PreviewStartEnvelope, StartExecError> {
        let Some(port) = record.port else {
            return Err(StartExecError::Fatal(invalid("admitted record lacks port")));
        };
        let ticket = ExecutorStartTicket {
            preview_key: key.to_string(),
            log_key: record.project_id.clone(),
            project_path: record.project_path.clone(),
            port,
            base_path: base_path.map(str::to_string),
            instance_id: record.instance_id.clone(),
        };
        let budget = Duration::from_secs(self.config.start_budget_secs);
        let started = tokio::time::timeout(budget, self.executor.start_local(&ticket)).await;
        let (pid, actual_port) = match started {
            Ok(Ok(pair)) => pair,
            Ok(Err(shared_types::PreviewExecutorError::PortInUse(detail))) => {
                return Err(StartExecError::PortInUse(port))
                    .inspect_err(|_| tracing::warn!(key, port, "preview port in use: {detail}"));
            }
            Ok(Err(error)) => {
                return Err(StartExecError::Fatal(unavailable(error.to_string())));
            }
            Err(_elapsed) => {
                // 预算超时：行保持 starting、执行器可能仍在安装——不自动重放；
                // 本机心跳扫描若发现已就绪将自愈 publish_running。
                return Err(StartExecError::Fatal(unavailable(
                    "preview start budget exceeded; instance left starting for heartbeat self-heal",
                )));
            }
        };
        let effective_base = base_path
            .map(normalize_base_path)
            .unwrap_or_else(|| default_base(actual_port));
        match self
            .store
            .publish_running(
                key,
                &record.operation_id,
                record.revision,
                pid,
                actual_port,
                Some(effective_base.as_str()),
            )
            .await
        {
            Ok(row) => {
                self.route_cache.invalidate(actual_port);
                Ok(Self::start_envelope(&row, "Development server started"))
            }
            Err(PreviewStoreError::Conflict(detail)) => {
                // 受理被并发接管：收掉我们刚起的进程（登记身份匹配才杀）。
                if let Err(kill_error) = self.executor.stop_local(key, &record.instance_id).await {
                    tracing::warn!(key, "post-supersede cleanup kill failed: {kill_error}");
                }
                Err(StartExecError::Fatal(conflict(format!(
                    "start admission superseded: {detail}"
                ))))
            }
            Err(error) => Err(StartExecError::Fatal(store_error(error))),
        }
    }

    /// 单实例协调停止（受理→派发→终态）。
    async fn coordinated_stop(
        &self,
        row: &PreviewInstanceRecord,
    ) -> Result<(), PreviewCoordinationError> {
        if self.settle_if_host_absent(row).await? {
            return Ok(());
        }
        // Resume an already accepted stop with its durable operation identity.
        // Re-dispatch is safe: executor stop is instance-bound and idempotent.
        let current = self
            .store
            .get(&row.preview_key)
            .await
            .map_err(store_error)?
            .ok_or_else(|| invalid("preview stop target disappeared"))?;
        if current.instance_id != row.instance_id {
            return Err(conflict(
                "stop target superseded by a newer preview instance",
            ));
        }
        let stopping = if current.state == PreviewInstanceState::Stopping {
            current
        } else {
            self.store
                .accept_stop(&current.preview_key, &new_id())
                .await
                .map_err(store_error)?
        };
        if !stopping.state.is_active() {
            return Ok(()); // 幂等：并发停止已完成或实例已终结
        }
        let outcome = match self.dispatch_stop(&stopping, &stopping.operation_id).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let detail = format!("stop dispatch outcome unknown: {error}");
                if let Err(store_failure) = self
                    .store
                    .mark_unknown(&stopping.preview_key, &stopping.instance_id, &detail)
                    .await
                {
                    return Err(unavailable(format!(
                        "{error}; failed to persist uncertain stop outcome: {store_failure}"
                    )));
                }
                if let Some(port) = stopping.port {
                    self.route_cache.invalidate(port);
                }
                return Err(error);
            }
        };
        match outcome {
            ExecutorStopOutcome::Stopped | ExecutorStopOutcome::NotRegistered => {
                self.store
                    .mark_stopped(
                        &stopping.preview_key,
                        &stopping.operation_id,
                        stopping.revision,
                    )
                    .await
                    .map_err(store_error)?;
                if let Some(port) = stopping.port {
                    self.route_cache.invalidate(port);
                }
                Ok(())
            }
            // 迟到旧操作打到新登记：不杀新实例；行由新实例持有，本操作作废。
            ExecutorStopOutcome::IdentityMismatch => {
                let detail = "stop target superseded by a newer instance on the host";
                self.store
                    .mark_unknown(&stopping.preview_key, &stopping.instance_id, detail)
                    .await
                    .map_err(store_error)?;
                if let Some(port) = stopping.port {
                    self.route_cache.invalidate(port);
                }
                Err(conflict(detail))
            }
        }
    }

    /// 停止派发：本机执行器 or 宿主 Pod 内部端点。
    async fn dispatch_stop(
        &self,
        stopping: &PreviewInstanceRecord,
        operation_id: &str,
    ) -> Result<ExecutorStopOutcome, PreviewCoordinationError> {
        if stopping.host_id == self.host.host_id {
            return self
                .executor
                .stop_local(&stopping.preview_key, &stopping.instance_id)
                .await
                .map_err(|e| unavailable(e.to_string()));
        }
        let Some(pod_ip) = stopping.pod_ip.clone() else {
            return Err(unavailable(format!(
                "stop target host {} lacks pod ip",
                stopping.host_id
            )));
        };
        self.dispatch
            .remote_stop(
                &pod_ip,
                &stopping.preview_key,
                &stopping.instance_id,
                operation_id,
                stopping.revision,
            )
            .await
            .map_err(unavailable)
    }

    /// verify 派发：本机 or 远端（不可达=Unavailable）。
    async fn verify_via_host(
        &self,
        row: &PreviewInstanceRecord,
    ) -> Result<ExecutorVerifyReport, PreviewCoordinationError> {
        if row.host_id == self.host.host_id {
            return self
                .executor
                .verify_local(&row.preview_key, &row.instance_id)
                .await
                .map_err(|e| unavailable(e.to_string()));
        }
        let Some(pod_ip) = row.pod_ip.clone() else {
            return Err(unavailable(format!(
                "verify target host {} lacks pod ip",
                row.host_id
            )));
        };
        self.dispatch
            .remote_verify(
                &pod_ip,
                &row.preview_key,
                &row.instance_id,
                self.config.remote_verify_timeout_secs,
            )
            .await
            .map_err(unavailable)
    }

    fn degraded(project_id: &str, reason: &str, message: &str) -> PreviewKeepAliveEnvelope {
        PreviewKeepAliveEnvelope {
            success: false,
            message: message.to_string(),
            project_id: project_id.to_string(),
            pid: None,
            port: None,
            action: None,
            reason: Some(reason.to_string()),
        }
    }

    fn rebuilt_envelope(env: PreviewStartEnvelope) -> PreviewKeepAliveEnvelope {
        PreviewKeepAliveEnvelope {
            success: true,
            message: "Development server started".to_string(),
            project_id: env.project_id,
            pid: Some(env.pid),
            port: Some(env.port),
            action: Some("start".to_string()),
            reason: None,
        }
    }

    /// 按端口定位活跃实例（先缓存后权威库）。
    async fn locate_by_port(
        &self,
        port: u16,
    ) -> Result<Option<PreviewInstanceRecord>, PreviewCoordinationError> {
        self.store
            .find_active_by_port(port)
            .await
            .map_err(store_error)
    }
}

/// 启动执行的内部错误分类（端口冲突可换端口重试）。
enum StartExecError {
    PortInUse(u16),
    Fatal(PreviewCoordinationError),
}

#[async_trait::async_trait]
impl PreviewCoordination for PreviewCoordinator {
    async fn start_dev(
        &self,
        req: PreviewStartRequest,
    ) -> Result<PreviewStartEnvelope, PreviewCoordinationError> {
        self.admit_and_start(&req.identity, req.base_path).await
    }

    async fn stop_dev(
        &self,
        req: &PreviewStopRequest,
    ) -> Result<PreviewStopEnvelope, PreviewCoordinationError> {
        let active = self.store.list_active().await.map_err(store_error)?;
        let targets: Vec<PreviewInstanceRecord> = active
            .into_iter()
            .filter(|r| r.project_id == req.project_id)
            .collect();
        if targets.is_empty() {
            return Ok(PreviewStopEnvelope {
                success: true,
                message: "No running process found".to_string(),
                project_id: req.project_id.clone(),
                reason: None,
            });
        }
        let mut all_ok = true;
        for row in targets {
            if let Err(error) = self.coordinated_stop(&row).await {
                tracing::warn!(
                    preview_key = %row.preview_key,
                    "coordinated stop failed: {error}"
                );
                all_ok = false;
            }
        }
        Ok(PreviewStopEnvelope {
            success: true,
            message: if all_ok {
                "Stopped".to_string()
            } else {
                "Partially stopped but continue execution".to_string()
            },
            project_id: req.project_id.clone(),
            reason: None,
        })
    }

    async fn restart_dev(
        &self,
        req: PreviewRestartRequest,
    ) -> Result<PreviewRestartEnvelope, PreviewCoordinationError> {
        let active = self.store.list_active().await.map_err(store_error)?;
        let targets: Vec<PreviewInstanceRecord> = active
            .into_iter()
            .filter(|r| r.project_id == req.identity.project_id)
            .collect();
        let mut stop_all_ok = true;
        for row in targets {
            if let Err(error) = self.coordinated_stop(&row).await {
                tracing::warn!(preview_key = %row.preview_key, "restart stop phase: {error}");
                stop_all_ok = false;
            }
        }
        if !stop_all_ok {
            return Err(unavailable(
                "restart stop phase failed on one or more instances",
            ));
        }
        let env = self.admit_and_start(&req.identity, req.base_path).await?;
        Ok(PreviewRestartEnvelope {
            success: true,
            message: "Development server restart successfully".to_string(),
            project_id: env.project_id,
            pid: env.pid,
            port: env.port,
        })
    }

    async fn keep_alive_dev(
        &self,
        req: &PreviewKeepAliveRequest,
    ) -> Result<PreviewKeepAliveEnvelope, PreviewCoordinationError> {
        let row = self.locate_by_port(req.port).await?;
        let Some(row) = row else {
            // 无活跃实例（旧语义=探活失败重建）→ 统一受理重建
            let env = self
                .admit_and_start(&req.identity, req.base_path.clone())
                .await?;
            return Ok(Self::rebuilt_envelope(env));
        };
        if row.project_id != req.identity.project_id {
            // 端口命中他人实例（复用/身份不符）：不动作、不重建
            return Ok(Self::degraded(
                &req.identity.project_id,
                degraded_reason::PORT_MISMATCH,
                "preview port belongs to another project",
            ));
        }
        match row.state {
            PreviewInstanceState::Ready if self.heartbeat_fresh(&row) => {
                self.activity
                    .record(&row.preview_key, &row.instance_id, chrono::Utc::now());
                Ok(PreviewKeepAliveEnvelope {
                    success: true,
                    message: "Development server is alive".to_string(),
                    project_id: row.project_id.clone(),
                    pid: row.pid,
                    port: row.port,
                    action: None,
                    reason: None,
                })
            }
            PreviewInstanceState::Ready => {
                // 心跳陈旧：回环 verify 宿主
                match self.verify_via_host(&row).await {
                    Ok(report) if report.identity_match && report.alive => {
                        note_store_result(
                            self.store
                                .refresh_heartbeat(&row.preview_key, &row.instance_id)
                                .await,
                        );
                        self.activity.record(
                            &row.preview_key,
                            &row.instance_id,
                            chrono::Utc::now(),
                        );
                        Ok(PreviewKeepAliveEnvelope {
                            success: true,
                            message: "Development server is alive".to_string(),
                            project_id: row.project_id.clone(),
                            pid: row.pid,
                            port: row.port,
                            action: None,
                            reason: None,
                        })
                    }
                    Ok(report) if report.identity_match => {
                        // 宿主确认死：收敛死实例后统一受理重建
                        note_store_result(
                            self.store
                                .mark_failed(
                                    &row.preview_key,
                                    &row.instance_id,
                                    row.revision,
                                    "host verified process dead",
                                )
                                .await,
                        );
                        let env = self
                            .admit_and_start(&req.identity, req.base_path.clone())
                            .await?;
                        Ok(Self::rebuilt_envelope(env))
                    }
                    Ok(_) => Ok(Self::degraded(
                        &req.identity.project_id,
                        degraded_reason::INSTANCE_UNKNOWN,
                        "preview host registration mismatch",
                    )),
                    Err(_) => Ok(Self::degraded(
                        &req.identity.project_id,
                        degraded_reason::HOST_UNAVAILABLE,
                        "preview host unreachable",
                    )),
                }
            }
            PreviewInstanceState::Starting => Err(conflict(
                "project dev server is already starting, please wait",
            )),
            PreviewInstanceState::Stopping => {
                if self.settle_if_host_absent(&row).await? {
                    let env = self
                        .admit_and_start(&req.identity, req.base_path.clone())
                        .await?;
                    Ok(Self::rebuilt_envelope(env))
                } else {
                    Ok(Self::degraded(
                        &req.identity.project_id,
                        degraded_reason::INSTANCE_UNKNOWN,
                        "preview instance is stopping; retry on next keep-alive",
                    ))
                }
            }
            PreviewInstanceState::Unknown => match self.recovery_evidence(&row).await {
                Some(evidence) => {
                    note_store_result(
                        self.store
                            .resolve_unknown_stopped(&row.preview_key, &row.instance_id, &evidence)
                            .await,
                    );
                    let env = self
                        .admit_and_start(&req.identity, req.base_path.clone())
                        .await?;
                    Ok(Self::rebuilt_envelope(env))
                }
                None => Ok(Self::degraded(
                    &req.identity.project_id,
                    degraded_reason::INSTANCE_UNKNOWN,
                    "preview instance state unknown and host pod still exists",
                )),
            },
            PreviewInstanceState::Stopped | PreviewInstanceState::Failed => {
                // 终态（旧语义=探活失败重建）→ 统一受理重建
                let env = self
                    .admit_and_start(&req.identity, req.base_path.clone())
                    .await?;
                Ok(Self::rebuilt_envelope(env))
            }
        }
    }

    async fn list_dev(&self) -> Result<Vec<PreviewListEntry>, PreviewCoordinationError> {
        let rows = self.store.list_active().await.map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|row| PreviewListEntry {
                project_id: row.project_id.clone(),
                pid: row.pid,
                port: row.port,
                state: format!("{:?}", row.state).to_lowercase(),
                host: row.pod_name.clone().or_else(|| Some(row.host_id.clone())),
                started_at: None,
            })
            .collect())
    }

    async fn read_dev_log(
        &self,
        project_id: &str,
        log_type: &str,
        start_index: usize,
    ) -> Result<ExecutorLogChunk, PreviewCoordinationError> {
        let rows = self.store.list_active().await.map_err(store_error)?;
        let Some(row) = rows.into_iter().find(|r| r.project_id == project_id) else {
            return Ok(ExecutorLogChunk {
                logs: Vec::new(),
                total_lines: 0,
                log_file_name: String::new(),
            });
        };
        if row.host_id == self.host.host_id {
            return self
                .executor
                .read_log_local(project_id, log_type, start_index)
                .await
                .map_err(|e| unavailable(e.to_string()));
        }
        let Some(pod_ip) = row.pod_ip.clone() else {
            return Err(unavailable(format!(
                "log target host {} lacks pod ip",
                row.host_id
            )));
        };
        self.dispatch
            .remote_log(&pod_ip, project_id, log_type, start_index)
            .await
            .map_err(unavailable)
    }

    async fn port_pool_status(&self) -> Result<PreviewPortPoolStatus, PreviewCoordinationError> {
        let rows = self.store.list_active().await.map_err(store_error)?;
        let allocations: Vec<PreviewPortAllocation> = rows
            .iter()
            .filter_map(|r| {
                r.port.map(|port| PreviewPortAllocation {
                    project_id: r.project_id.clone(),
                    port,
                })
            })
            .collect();
        Ok(PreviewPortPoolStatus {
            port_range: format!("{}-{}", PREVIEW_PORT_MIN, PREVIEW_PORT_MAX),
            total_allocated: allocations.len(),
            allocations,
        })
    }

    async fn resolve_route(&self, port: u16) -> PreviewRouteResolution {
        if !is_preview_port(port) {
            return PreviewRouteResolution::NotPreview;
        }
        if let Some(cached) = self.route_cache.get(port) {
            return cached;
        }
        match self.store.find_active_by_port(port).await {
            Ok(Some(row)) => {
                let resolution = if row.host_id == self.host.host_id {
                    PreviewRouteResolution::Local {
                        instance_id: row.instance_id.clone(),
                        port,
                    }
                } else if let Some(host_ip) = row.pod_ip.clone() {
                    PreviewRouteResolution::Forward {
                        instance_id: row.instance_id.clone(),
                        port,
                        host_ip,
                    }
                } else {
                    // 远端宿主行缺 pod_ip（异常/单机形态）：降级 legacy，不缓存
                    return PreviewRouteResolution::Unavailable;
                };
                self.route_cache.put_positive(port, resolution.clone());
                resolution
            }
            Ok(None) => {
                self.route_cache.put_negative(port);
                PreviewRouteResolution::NotPreview
            }
            Err(error) => {
                // 权威库不可用：不缓存，降级 legacy（不劣于现状）
                tracing::warn!(port, "preview route resolve degraded: {error}");
                PreviewRouteResolution::Unavailable
            }
        }
    }

    async fn invalidate_route(&self, port: u16) {
        self.route_cache.invalidate(port);
    }

    async fn check_forward(&self, instance_id: &str, port: u16) -> PreviewForwardCheck {
        let Ok(Some(row)) = self.store.find_active_by_port(port).await else {
            // 权威库错误或实例不存在：410（调用方重解析后负缓存/NotFound 收敛）
            return PreviewForwardCheck::IdentityMismatch;
        };
        if row.instance_id != instance_id
            || row.port != Some(port)
            || row.host_id != self.host.host_id
        {
            return PreviewForwardCheck::IdentityMismatch;
        }
        // 登记匹配即放行（纯内存查找，无探活——存活是心跳轮询的职责；
        // 转发到已死 vite 得到连接拒绝，语义与现状一致）。
        match self
            .executor
            .registration_matches(&row.preview_key, &row.instance_id)
            .await
        {
            Ok(true) => PreviewForwardCheck::Allowed,
            _ => PreviewForwardCheck::NotReady,
        }
    }

    async fn internal_stop(
        &self,
        preview_key: &str,
        instance_id: &str,
        operation_id: &str,
        revision: i64,
    ) -> Result<ExecutorStopOutcome, PreviewCoordinationError> {
        let row = self.store.get(preview_key).await.map_err(store_error)?;
        let authorized = row.is_some_and(|row| {
            row.instance_id == instance_id
                && row.operation_id == operation_id
                && row.revision == revision
                && row.state == PreviewInstanceState::Stopping
                && row.host_id == self.host.host_id
        });
        if !authorized {
            return Ok(ExecutorStopOutcome::IdentityMismatch);
        }
        self.executor
            .stop_local(preview_key, instance_id)
            .await
            .map_err(|e| unavailable(e.to_string()))
    }

    async fn internal_verify(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorVerifyReport, PreviewCoordinationError> {
        self.executor
            .verify_local(preview_key, instance_id)
            .await
            .map_err(|e| unavailable(e.to_string()))
    }
}

/// 协调路径的存储写回失败（CAS 让位/幂等收敛）只记日志。
fn note_store_result(result: Result<impl Send, PreviewStoreError>) {
    if let Err(error) = result {
        tracing::debug!("preview store write skipped (converged by CAS): {error}");
    }
}

#[cfg(test)]
mod tests {
    use shared_types::{PreviewExecutor, PreviewExecutorError, PreviewLifecycleStore};

    use super::*;
    use crate::{InProcessPreviewStore, SingleInstanceEvidence};

    struct TestExecutor {
        fail_stop: bool,
    }

    impl TestExecutor {
        fn new(fail_stop: bool) -> Self {
            Self { fail_stop }
        }
    }

    #[async_trait::async_trait]
    impl PreviewExecutor for TestExecutor {
        async fn start_local(
            &self,
            ticket: &ExecutorStartTicket,
        ) -> Result<(i64, u16), PreviewExecutorError> {
            Ok((42, ticket.port))
        }

        async fn stop_local(
            &self,
            _preview_key: &str,
            _instance_id: &str,
        ) -> Result<ExecutorStopOutcome, PreviewExecutorError> {
            if self.fail_stop {
                Err(PreviewExecutorError::Failed(
                    "injected stop failure".to_string(),
                ))
            } else {
                Ok(ExecutorStopOutcome::NotRegistered)
            }
        }

        async fn verify_local(
            &self,
            _preview_key: &str,
            _instance_id: &str,
        ) -> Result<ExecutorVerifyReport, PreviewExecutorError> {
            Ok(ExecutorVerifyReport {
                identity_match: true,
                alive: true,
                pid: Some(1),
                port: Some(PREVIEW_PORT_MIN),
            })
        }

        async fn registration_matches(
            &self,
            _preview_key: &str,
            _instance_id: &str,
        ) -> Result<bool, PreviewExecutorError> {
            Ok(true)
        }

        async fn read_log_local(
            &self,
            _log_key: &str,
            _log_type: &str,
            _start_index: usize,
        ) -> Result<ExecutorLogChunk, PreviewExecutorError> {
            Ok(ExecutorLogChunk {
                logs: Vec::new(),
                total_lines: 0,
                log_file_name: String::new(),
            })
        }
    }

    fn identity(project_id: &str) -> PreviewProjectIdentity {
        PreviewProjectIdentity {
            project_id: project_id.to_string(),
            tenant_id: None,
            space_id: None,
            isolation_type: None,
            resolved_path: format!("/tmp/{project_id}"),
        }
    }

    fn coordinator(
        store: Arc<InProcessPreviewStore>,
        executor: Arc<TestExecutor>,
    ) -> PreviewCoordinator {
        let config = CoordinatorConfig {
            start_budget_secs: 1,
            ..CoordinatorConfig::default()
        };
        PreviewCoordinator::new(
            store,
            executor,
            Arc::new(SingleInstanceEvidence),
            "test-preview-token".to_string(),
            config,
        )
    }

    async fn seed_ready(
        store: &InProcessPreviewStore,
        identity: &PreviewProjectIdentity,
        host: PreviewHostIdentity,
        operation_id: &str,
        instance_id: &str,
    ) -> PreviewInstanceRecord {
        let key = compute_key(identity);
        let accepted = store
            .accept_start(AcceptStartInput {
                preview_key: key.clone(),
                project_id: identity.project_id.clone(),
                project_path: identity.resolved_path.clone(),
                host,
                operation_id: operation_id.to_string(),
                instance_id: instance_id.to_string(),
                requested_port: None,
                recover_unknown_evidence: None,
            })
            .await
            .expect("seed start admission");
        let AcceptStartOutcome::Admitted(starting) = accepted else {
            panic!("seed start must be admitted");
        };
        let port = starting.port.expect("seed start allocates port");
        store
            .publish_running(&key, operation_id, starting.revision, 42, port, None)
            .await
            .expect("seed ready instance")
    }

    #[tokio::test]
    async fn restart_recovers_stopping_instance_owned_by_deleted_host() {
        let store = Arc::new(InProcessPreviewStore::new());
        let executor = Arc::new(TestExecutor::new(false));
        let coordinator = coordinator(Arc::clone(&store), Arc::clone(&executor));
        let identity = identity("orphaned-preview");
        let ready = seed_ready(
            store.as_ref(),
            &identity,
            PreviewHostIdentity {
                host_id: "deleted-pod-uid:old-boot".to_string(),
                pod_name: Some("deleted-rcoder-pod".to_string()),
                pod_ip: Some("10.0.0.8".to_string()),
            },
            "old-start-operation",
            "old-instance",
        )
        .await;
        let stopping = store
            .accept_stop(&ready.preview_key, "abandoned-stop-operation")
            .await
            .expect("seed abandoned stop");
        assert_eq!(stopping.state, PreviewInstanceState::Stopping);

        let restarted = coordinator
            .restart_dev(PreviewRestartRequest {
                identity,
                base_path: None,
            })
            .await
            .expect("restart must recover deleted host and start a replacement");

        assert!(restarted.success);
        let current = store
            .get(&ready.preview_key)
            .await
            .expect("read current preview")
            .expect("replacement preview exists");
        assert_eq!(current.state, PreviewInstanceState::Ready);
        assert_eq!(current.host_id, coordinator.host().host_id);
        assert_ne!(current.instance_id, ready.instance_id);
    }

    #[tokio::test]
    async fn stop_dispatch_failure_does_not_leave_instance_stopping() {
        let store = Arc::new(InProcessPreviewStore::new());
        let executor = Arc::new(TestExecutor::new(true));
        let coordinator = coordinator(Arc::clone(&store), executor);
        let identity = identity("failed-stop-preview");
        let ready = seed_ready(
            store.as_ref(),
            &identity,
            coordinator.host().clone(),
            "start-operation",
            "local-instance",
        )
        .await;

        let error = coordinator
            .coordinated_stop(&ready)
            .await
            .expect_err("injected stop failure must propagate");
        assert!(error.to_string().contains("injected stop failure"));

        let current = store
            .get(&ready.preview_key)
            .await
            .expect("read failed stop state")
            .expect("preview remains recorded");
        assert_eq!(current.state, PreviewInstanceState::Unknown);
        assert!(
            current
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("injected stop failure"))
        );
    }
}
