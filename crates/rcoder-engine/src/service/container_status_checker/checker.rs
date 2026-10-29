//! ContainerStatusChecker 主体：周期检查流程、容器存在性分派、启动入口。

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{Instrument, debug, info, warn};

use crate::app_state::AppState;
use container_runtime_api::ContainerRuntimeError;
use shared_types::{
    AgentStatusObservation, ComputerOwnerMismatch, ContainerBasicInfo, ContainerWorkloadReplaced,
    ProjectAndContainerInfo, ProjectStore as _,
};

use super::state::{ContainerHealthState, ContainerStatusCheckerConfig, HealthScope};

/// 解析结论：要么是可核实的探针目标，要么是明确的不探测保留。
enum StatusTarget {
    /// 探针地址 + 解析时捕获的观察身份（owned 值；RPC 在途的注册变更——
    /// 包括共享 ContainerEntry 的原地刷新——不会改写捕获结果）。
    Probe {
        addr: String,
        captured: CapturedObservation,
    },
    /// 不发 RPC 的保留结论：缺席/未知/不可核实的换代，注册原样保留。
    Keep { reason: String },
}

/// 一次探针的观察身份：物理 UID/workload + 注册代次/revision，全部在
/// 解析时以 owned 值捕获。内存后端的 ContainerEntry 与注册共享同一 Arc、
/// 同名换代会原地刷新，因此 RPC 返回后再读快照会读到新代次——必须捕获。
struct CapturedObservation(AgentStatusObservation);

impl CapturedObservation {
    fn capture(info: &ProjectAndContainerInfo) -> Option<Self> {
        AgentStatusObservation::capture(info, false).map(Self)
    }
    fn matches_basic(&self, basic: &ContainerBasicInfo) -> bool {
        self.0.container_id == basic.container_id
            && self.0.workload_uid == basic.workload_uid
            && self.0.container_name == basic.container_name
    }

    fn scope(&self) -> HealthScope {
        HealthScope {
            container_id: self.0.container_id.clone(),
            workload_uid: self.0.workload_uid.clone(),
        }
    }
    fn observation(&self, active: bool) -> AgentStatusObservation {
        AgentStatusObservation {
            active,
            ..self.0.clone()
        }
    }
}

/// 从 runtime 观测构造身份 basic：物理身份字段取观测值，project_id 用注册
/// 定位键（与运行时 build_container_basic_info 的约定一致）。
fn observed_basic(
    identifier: &str,
    found: &container_runtime_api::RuntimeContainerInfo,
) -> ContainerBasicInfo {
    ContainerBasicInfo {
        container_id: found.container_id.clone(),
        container_name: found.container_name.clone(),
        container_ip: found.container_ip.clone(),
        internal_port: shared_types::GRPC_DEFAULT_PORT,
        external_port: 0,
        project_id: identifier.to_string(),
        status: format!("{:?}", found.status).to_lowercase(),
        created_at: found.created_at,
        service_url: format!(
            "http://{}:{}",
            found.container_ip,
            shared_types::GRPC_DEFAULT_PORT
        ),
        workload_uid: found.workload_uid.clone(),
    }
}

/// 格式化日期时间为标准格式（如：2026-01-12 15:04:30）
fn format_datetime(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 格式化相对时间（如：5分钟前）
fn format_relative_time(dt: DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(dt);

    if duration.num_seconds() < 60 {
        format!("{}s ago", duration.num_seconds())
    } else if duration.num_minutes() < 60 {
        format!("{}m ago", duration.num_minutes())
    } else if duration.num_hours() < 24 {
        format!("{}h ago", duration.num_hours())
    } else {
        format!("{}d ago", duration.num_days())
    }
}

/// Computer 换代恢复被拒的完整脱敏 cause 链。anyhow 的 Display 只剩最外层
/// context（"Observe Computer workload replacement"），会同时隐藏
/// [`ContainerWorkloadReplaced`] typed 冲突与 runtime 底层原因；`{error:#}`
/// 按仓库惯例单行展开整链，再经 [`shared_types::sanitize_error_text`] 脱敏
/// 凭据并限长（有界诊断）。
fn replacement_rejection_detail(error: &anyhow::Error) -> String {
    shared_types::sanitize_error_text(&format!("{error:#}"))
}

/// Computer 换代恢复被拒的 typed 类别：优先透出 durable 注册闸门已构造的
/// typed 冲突契约（[`ContainerWorkloadReplaced`] / [`ComputerOwnerMismatch`]），
/// 否则按 cause 链中的 runtime 变体分类；两者都不可得（如存储刷新失败、
/// 重试围栏）时保留显式标签，不冒领 runtime 类别。
fn replacement_rejection_category(error: &anyhow::Error) -> &'static str {
    const UNCLASSIFIED: &str = "computer.replacement.rejected.unclassified";
    if error.downcast_ref::<ContainerWorkloadReplaced>().is_some() {
        return shared_types::error_codes::CONTAINER_WORKLOAD_REPLACED;
    }
    if let Some(mismatch) = error.downcast_ref::<ComputerOwnerMismatch>() {
        return mismatch.reason_code();
    }
    match error.downcast_ref::<ContainerRuntimeError>() {
        Some(runtime) => container_runtime_api::runtime_error_code(runtime).0,
        None => UNCLASSIFIED,
    }
}

/// 容器状态检查器
#[derive(Clone)]
pub(super) struct ContainerStatusChecker {
    pub(super) config: ContainerStatusCheckerConfig,
    pub(super) state: Arc<AppState>,
    /// 容器健康状态映射 (lookup_key -> health_state)
    pub(super) health_states: Arc<DashMap<(String, HealthScope), ContainerHealthState>>,
    /// 测试注入的探针端口（真实 Tonic 服务绑动态 loopback 端口时使用）；
    /// 生产路径恒用默认 gRPC 端口。
    #[cfg(test)]
    pub(super) probe_port_override: Option<u16>,
}

impl ContainerStatusChecker {
    /// 创建新的状态检查器
    pub(super) fn new(config: ContainerStatusCheckerConfig, state: Arc<AppState>) -> Self {
        Self {
            config,
            state,
            health_states: Arc::new(DashMap::new()),
            #[cfg(test)]
            probe_port_override: None,
        }
    }

    /// 测试专用：把探针指向动态端口，避免占用固定 GRPC_DEFAULT_PORT。
    #[cfg(test)]
    pub(super) fn with_probe_port(mut self, port: u16) -> Self {
        self.probe_port_override = Some(port);
        self
    }

    /// 检查所有容器的状态
    ///
    /// 🚀 使用并发批处理优化：每批最多 10 个容器并发检查
    /// 100 个容器从最差 300s (5分钟) 降低到 ~30s
    async fn check_all_containers(&self) -> anyhow::Result<()> {
        use futures_util::future::join_all;

        // 收集所有需要检查的容器（创建快照）
        let containers: Vec<(String, Arc<ProjectAndContainerInfo>)> = self.state.projects.iter();

        if containers.is_empty() {
            debug!("[STATUS_CHECKER] No containers to check");
            return Ok(());
        }

        info!(
            "[STATUS_CHECKER] Starting to check {} containers",
            containers.len()
        );

        let total_count = containers.len();
        let mut skipped = 0;

        // 🚀 预过滤：收集需要检查的容器（should_skip_check 是同步操作）。
        // 退避按物理代次判定：同逻辑键已换代的实例不受前任失败窗口约束。
        let to_check: Vec<_> = containers
            .into_iter()
            .filter_map(|(_project_id, container_info)| {
                let lookup_key = container_info.container_key().to_string();
                let Some(scope) = container_info
                    .container_info()
                    .map(|basic| HealthScope::of(&basic))
                else {
                    debug!("[STATUS_CHECKER] Container info not found: {}", lookup_key);
                    return None;
                };
                if self.should_skip_check(&lookup_key, &scope) {
                    skipped += 1;
                    debug!(
                        "[STATUS_CHECKER] Skipping check (recently failed): {}",
                        lookup_key
                    );
                    None
                } else {
                    Some((lookup_key, container_info))
                }
            })
            .collect();

        let checked = to_check.len();
        let mut updated = 0;
        let mut failed = 0;

        // 🚀 并发批处理：每批最多 10 个容器
        let batch_size = 10;
        for chunk in to_check.chunks(batch_size) {
            let futures: Vec<_> = chunk
                .iter()
                .map(|(lookup_key, container_info)| {
                    let checker = self.clone();
                    let lookup_key = lookup_key.clone();
                    let container_info = container_info.clone();
                    async move {
                        checker
                            .check_single_container(&lookup_key, &container_info)
                            .await
                    }
                })
                .collect();

            let results = join_all(futures).await;
            for result in results {
                match result {
                    Ok(true) => updated += 1,
                    Ok(false) => {} // 容器空闲或未更新
                    Err(_) => failed += 1,
                }
            }
        }

        info!(
            "[STATUS_CHECKER] Check completed: total={}, checked={}, skipped={}, updated={}, failed={}",
            total_count, checked, skipped, updated, failed
        );

        Ok(())
    }

    /// 检查单个容器
    ///
    /// 流程：解析可核实目标（必要时受控重登记）→ 探针 → 按观察代次条件应用。
    /// 返回是否更新了活动时间；未核实身份前不向注册旧地址发 RPC。
    pub(super) async fn check_single_container(
        &self,
        lookup_key: &str,
        container_info: &Arc<ProjectAndContainerInfo>,
    ) -> anyhow::Result<bool> {
        let budget = self
            .config
            .query_timeout
            .saturating_add(Duration::from_secs(shared_types::GRPC_CONNECT_TIMEOUT_SECS));
        time::timeout(
            budget,
            self.check_single_container_inner(lookup_key, container_info),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Agent status observation exceeded its total budget"))?
    }

    async fn check_single_container_inner(
        &self,
        lookup_key: &str,
        container_info: &Arc<ProjectAndContainerInfo>,
    ) -> anyhow::Result<bool> {
        // 获取容器信息
        let registered = match container_info.container_info() {
            Some(c) => c,
            None => {
                debug!("[STATUS_CHECKER] Container info not found: {}", lookup_key);
                return Ok(false);
            }
        };
        let Some(family) = container_info.service_type() else {
            debug!(
                "[STATUS_CHECKER] Agent service family is missing: {}",
                lookup_key
            );
            return Ok(false);
        };

        // 获取最后激活时间用于日志显示
        let last_activity = container_info.last_activity();
        let last_activity_str = format_datetime(last_activity);
        let relative_time_str = format_relative_time(last_activity);

        // 提取 user_id（lookup_key 可能是 user_id 或 project_id）
        let user_id = container_info
            .user_id()
            .map(|s| s.to_string())
            .unwrap_or_else(|| lookup_key.to_string());

        let project_id = container_info.project_id().to_string();

        // 解析目标：缺席/未知/不可核实的换代一律保留注册，不探测旧地址
        let (grpc_addr, captured) = match self
            .resolve_status_target(family, lookup_key, container_info)
            .await?
        {
            StatusTarget::Probe { addr, captured } => (addr, captured),
            StatusTarget::Keep { reason } => {
                let scope = HealthScope::of(&registered);
                let registered_addr = self
                    .probe_addr(&registered.container_ip, &registered.container_name)
                    .unwrap_or_else(|_| {
                        format!(
                            "{}:{}",
                            registered.container_ip,
                            shared_types::GRPC_DEFAULT_PORT
                        )
                    });
                let error = anyhow::anyhow!("status target unresolved: {}", reason);
                self.record_failure(lookup_key, &scope, &registered_addr, &error);
                return Err(error);
            }
        };
        let scope = captured.scope();

        // 查询容器状态
        match crate::grpc::status_query::query_container_status(
            &self.state.grpc_pool,
            &grpc_addr,
            &user_id,
            &project_id,
            self.config.query_timeout,
        )
        .await
        .map(|status| {
            debug!(
                "[STATUS_CHECKER] Container status: user_id={}, is_active={}, active_tasks={}, status={}, last_activity={} ({})",
                user_id,
                status.is_active,
                status.active_tasks,
                status.status,
                last_activity_str,
                relative_time_str
            );
            crate::grpc::status_query::is_agent_active(&status)
        }) {
            Ok(active) => {
                // 按观察代次条件应用：换代/缺席即丢弃，旧回调不写新代次
                let observation = captured.observation(active);
                if !self
                    .state
                    .projects
                    .apply_agent_status_observation(&project_id, &observation)
                {
                    debug!(
                        "[STATUS_CHECKER] Observation superseded by a newer registration; result discarded: container_key={}, project_id={}",
                        lookup_key, project_id
                    );
                    return Ok(false);
                }
                self.record_success(lookup_key, &scope);
                if active {
                    debug!(
                        "[STATUS_CHECKER] Container is active, updated activity time and status: container_key={}, project_id={}",
                        lookup_key, project_id
                    );
                    Ok(true)
                } else {
                    debug!(
                        "[STATUS_CHECKER] Container is idle, updated status to Idle: container_key={}, project_id={}",
                        lookup_key, project_id
                    );
                    Ok(false)
                }
            }
            Err(e) => {
                // 目标在解析时已核实；失败按该物理代次计数（连接清理由
                // record_failure 的阈值逻辑完成），不做缺席推断。
                self.record_failure(lookup_key, &scope, &grpc_addr, &e);
                Err(e)
            }
        }
    }

    /// 解析可核实的状态目标。只有与当前登记一致的物理身份（或 Computer 族
    /// 经运行时证明完成受控重登记后的新实例）才返回探针目标；其余返回
    /// Keep —— 缺席/未知/不可核实的换代不探测旧地址，也不推断清理。
    async fn resolve_status_target(
        &self,
        family: shared_types::ServiceType,
        lookup_key: &str,
        registration: &Arc<ProjectAndContainerInfo>,
    ) -> anyhow::Result<StatusTarget> {
        let registered = registration
            .container_info()
            .ok_or_else(|| anyhow::anyhow!("registration has no container info"))?;
        // 枚举穷尽：Userapp 族没有 runtime 身份查找源（app_manager 独立管理），
        // 保持注册地址直探测；其余族先核实当前物理身份。
        let captured = CapturedObservation::capture(registration)
            .ok_or_else(|| anyhow::anyhow!("registration has no container info"))?;
        if !captured.matches_basic(&registered) {
            return Ok(StatusTarget::Keep {
                reason: "registration changed during identity capture".into(),
            });
        }
        match family {
            shared_types::ServiceType::Userapp => {
                let addr = self.probe_addr(&registered.container_ip, &registered.container_name);
                return match addr {
                    Ok(addr) => Ok(StatusTarget::Probe { addr, captured }),
                    Err(error) => Ok(StatusTarget::Keep {
                        reason: format!("published address is not reachable: {error}"),
                    }),
                };
            }
            shared_types::ServiceType::WebAgentRunner
            | shared_types::ServiceType::ComputerAgentRunner
            | shared_types::ServiceType::ComputerNormalProject
            | shared_types::ServiceType::UserappBuilder => {}
        }
        let identifier = match family.container_identifier(
            registration.pod_id(),
            registration.user_id(),
            Some(registration.project_id()),
        ) {
            Ok(identifier) => identifier.to_string(),
            Err(error) => {
                return Ok(StatusTarget::Keep {
                    reason: format!("registered identity is incomplete: {error}"),
                });
            }
        };
        let found = match self
            .state
            .runtime()
            .find_container(&identifier, &family)
            .await
        {
            Ok(found) => found,
            Err(error) => {
                debug!(container_key = lookup_key, %error,
                    "[STATUS_CHECKER] Runtime identity lookup failed; no probe without verification");
                return Ok(StatusTarget::Keep {
                    reason: format!("runtime identity lookup failed: {error}"),
                });
            }
        };
        let Some(found) = found else {
            info!(
                container_key = lookup_key,
                "[STATUS_CHECKER] No runtime candidate observed; registration retained for authoritative reconciliation"
            );
            return Ok(StatusTarget::Keep {
                reason: "no live runtime candidate for the registered identity".into(),
            });
        };
        if found.container_id == registered.container_id
            && found.workload_uid == registered.workload_uid
            && found.container_name == registered.container_name
        {
            // 同物理实例：地址以 runtime 观测为准（IP 可能漂移），身份与代次
            // 仍以解析时捕获的注册快照为准。
            return match self.probe_addr(&found.container_ip, &registered.container_name) {
                Ok(addr) => Ok(StatusTarget::Probe { addr, captured }),
                Err(error) => Ok(StatusTarget::Keep {
                    reason: format!("published address is not reachable: {error}"),
                }),
            };
        }
        // 物理身份已变化。观测 basic 的身份字段来自 runtime（project_id 用
        // 注册的定位键，workload/container UID 用观测值）。
        let observed = observed_basic(&identifier, &found);
        let change = shared_types::AgentRegistrationChange::between(Some(&registered), &observed);
        match family {
            shared_types::ServiceType::ComputerAgentRunner
            | shared_types::ServiceType::ComputerNormalProject => match change {
                Some(_) => {
                    match self
                        .state
                        .recover_computer_registration_observed(registration, &observed)
                        .await
                    {
                        Ok(Some(fresh)) => {
                            let captured =
                                CapturedObservation::capture(&fresh).ok_or_else(|| {
                                    anyhow::anyhow!("recovered registration has no container info")
                                })?;
                            let basic = fresh.container_info().ok_or_else(|| {
                                anyhow::anyhow!("recovered registration has no container info")
                            })?;
                            if !captured.matches_basic(&basic) {
                                return Ok(StatusTarget::Keep {
                                    reason: "recovered registration changed before probe dispatch"
                                        .into(),
                                });
                            }
                            let addr = self.probe_addr(&basic.container_ip, &basic.container_name);
                            match addr {
                                Ok(addr) => Ok(StatusTarget::Probe { addr, captured }),
                                Err(error) => Ok(StatusTarget::Keep {
                                    reason: format!(
                                        "recovered registration address is not reachable: {error}"
                                    ),
                                }),
                            }
                        }
                        // durable 未提交（并发/延迟）：保持并重试，不探测任何地址
                        Ok(None) => Ok(StatusTarget::Keep {
                            reason: "computer replacement registration is not committed yet".into(),
                        }),
                        Err(error) => {
                            // 诊断增强（保护语义不变，不放开接管）：anyhow 的
                            // Display 只剩最外层 context，无法归因 runtime 拒绝
                            // 原因。完整脱敏 cause 链 + typed 类别 + 新旧物理
                            // 身份；WARN 级别与保留注册行为均不变。
                            let rejection = replacement_rejection_detail(&error);
                            warn!(container_key = lookup_key,
                                rejection = %rejection,
                                category = replacement_rejection_category(&error),
                                registered_container_id = %registered.container_id,
                                registered_workload_uid = ?registered.workload_uid,
                                observed_container_id = %observed.container_id,
                                observed_workload_uid = ?observed.workload_uid,
                                "[STATUS_CHECKER] Computer replacement recovery rejected; registration retained");
                            // reason 会进入 record_failure 的失败日志与向上传播的
                            // 错误文本：同样携带完整链，不再只剩最外层 context。
                            Ok(StatusTarget::Keep {
                                reason: format!(
                                    "computer replacement recovery rejected: {rejection}"
                                ),
                            })
                        }
                    }
                }
                // 换代证据不完整（如 workload 身份缺失）：不可借无法验证的动作
                None => Ok(StatusTarget::Keep {
                    reason: "observed replacement lacks a verifiable physical identity".into(),
                }),
            },
            // Web/UserappBuilder 族没有受控换代验证动作：保持既有登记协议，
            // 换代由前台 ensure/业务路径收敛，这里只观察留痕。
            shared_types::ServiceType::WebAgentRunner
            | shared_types::ServiceType::UserappBuilder => {
                warn!(container_key = lookup_key,
                    registered_uid = %registered.container_id,
                    observed_uid = %found.container_id,
                    "[STATUS_CHECKER] Registered physical identity was replaced; no probe and no automatic takeover");
                Ok(StatusTarget::Keep {
                    reason: "registered physical identity was replaced; awaiting controlled re-registration"
                        .into(),
                })
            }
            // Userapp 已在函数开头返回；防御性保留，不给它借身份解析
            shared_types::ServiceType::Userapp => Ok(StatusTarget::Keep {
                reason: "userapp does not participate in agent identity resolution".into(),
            }),
        }
    }

    /// 探针地址（deploy-host 经注册表键解析；否则容器 IP 直连）。
    /// 连接池 get_client 自带 connect/request 超时，与 RPC 超时共同构成总预算。
    fn probe_addr(&self, container_ip: &str, container_name: &str) -> anyhow::Result<String> {
        let port = self.probe_port();
        #[cfg(feature = "deploy-host")]
        {
            if shared_types::is_deploy_host() {
                // 发布地址未就绪不回退直连 Pod IP——那正是未经核实的旧地址形态
                return Ok(
                    shared_types::published::resolve_published_addr(container_name, port)?
                        .to_string(),
                );
            }
        }
        #[cfg(not(feature = "deploy-host"))]
        let _ = container_name;
        Ok(format!("{}:{}", container_ip, port))
    }

    fn probe_port(&self) -> u16 {
        #[cfg(test)]
        if let Some(port) = self.probe_port_override {
            return port;
        }
        shared_types::GRPC_DEFAULT_PORT
    }

    /// Lookup presence is diagnostic, never a physical-exit receipt.
    /// （新流程在 resolve_status_target 内做同一族分派并消费完整身份；
    /// 本方法保留给族分派契约测试。）
    #[cfg(test)]
    pub(super) async fn check_container_exists(
        &self,
        container_info: &Arc<ProjectAndContainerInfo>,
        _grpc_addr: &str,
    ) -> anyhow::Result<bool> {
        let family = container_info
            .service_type()
            .ok_or_else(|| anyhow::anyhow!("Agent service family is missing"))?;
        match family {
            shared_types::ServiceType::WebAgentRunner
            | shared_types::ServiceType::ComputerAgentRunner
            | shared_types::ServiceType::ComputerNormalProject
            | shared_types::ServiceType::UserappBuilder => {}
            shared_types::ServiceType::Userapp => {
                anyhow::bail!("Production UserApp does not participate in Agent status lookup")
            }
        }
        let identifier = family.container_identifier(
            container_info.pod_id(),
            container_info.user_id(),
            Some(container_info.project_id()),
        )?;
        Ok(self
            .state
            .runtime()
            .find_container(identifier, &family)
            .await?
            .is_some())
    }
}

/// 启动容器状态检查任务
///
/// 定期查询所有容器的 Agent Runner 状态，如果容器有活跃任务则更新活动时间
pub fn start_container_status_checker(
    config: ContainerStatusCheckerConfig,
    state: Arc<AppState>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    info!(
        "[STATUS_CHECKER] Starting container status checker: interval={}s, failure_threshold={}, skip_duration={}s",
        config.check_interval.as_secs(),
        config.failure_threshold,
        config.skip_duration.as_secs()
    );

    let checker = Arc::new(ContainerStatusChecker::new(config.clone(), state));

    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut interval = time::interval(config.check_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let mut cleanup_counter = 0;
        let cleanup_interval = 10; // 每 10 次检查清理一次健康状态

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // 执行容器状态检查（bg_status_check span：周期任务可观测）
                    let result = tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        result = checker.check_all_containers().instrument(tracing::info_span!("bg_status_check")) => result,
                    };
                    if let Err(e) = result {
                        warn!("[STATUS_CHECKER] Container status check failed: {}", e);
                    }

                    // 定期清理过期的健康状态
                    cleanup_counter += 1;
                    if cleanup_counter >= cleanup_interval {
                        checker.cleanup_stale_health_states();
                        cleanup_counter = 0;
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("[STATUS_CHECKER] shutdown");
                    break;
                }
            }
        }
    })
}
