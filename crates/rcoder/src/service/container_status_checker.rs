//! 容器状态检查器
//!
//! 定期查询 Agent Runner 的容器状态，如果容器有活跃任务则更新活动时间。
//! 这样可以防止正在执行长时间任务的容器被清理任务误判为闲置而销毁。
//!
//! 注意：本模块由 binary (main.rs) 使用，lib 内部不直接调用，因此整体
//! 抑制 dead_code 警告。

#![allow(dead_code)]
//!
//! ## 优化特性
//!
//! 1. **Docker 主动查询**：gRPC 失败时主动查询 Docker 容器是否存在
//! 2. **失败计数器**：为每个容器维护健康状态，记录连续失败次数
//! 3. **智能跳过**：连续失败超过阈值后暂时跳过检查
//! 4. **自动清理**：容器不存在时立即清理 gRPC 连接池和健康状态
//! 5. **分级日志**：根据失败次数输出不同级别的日志

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{Instrument, debug, info, warn};

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

use crate::grpc::GrpcChannelPool;
use crate::router::AppState;
// 存储契约 trait：state.projects（ProjectStoreBackend 枚举）上的方法经此解析
use shared_types::ProjectStore as _;
use shared_types::grpc::GetContainerStatusRequest;

/// 容器健康状态
#[derive(Debug, Clone)]
struct ContainerHealthState {
    /// 连续失败次数
    consecutive_failures: u32,
    /// 首次失败时间
    first_failure_time: Option<DateTime<Utc>>,
    /// 最后检查时间
    last_check_time: DateTime<Utc>,
    /// 最后成功时间
    last_success_time: Option<DateTime<Utc>>,
}

impl ContainerHealthState {
    /// 创建新的健康状态
    fn new() -> Self {
        Self {
            consecutive_failures: 0,
            first_failure_time: None,
            last_check_time: Utc::now(),
            last_success_time: Some(Utc::now()),
        }
    }

    /// 创建失败状态
    fn new_failed() -> Self {
        let now = Utc::now();
        Self {
            consecutive_failures: 1,
            first_failure_time: Some(now),
            last_check_time: now,
            last_success_time: None,
        }
    }
}

/// 容器状态检查配置
#[derive(Debug, Clone)]
pub struct ContainerStatusCheckerConfig {
    /// 检查间隔（默认 30 秒）
    pub check_interval: Duration,
    /// 查询超时（默认 5 秒）
    pub query_timeout: Duration,
    /// 连续失败阈值（默认 3 次）
    pub failure_threshold: u32,
    /// 失败容器跳过时间（默认 5 分钟）
    pub skip_duration: Duration,
    /// 健康状态重置周期（默认 30 分钟）
    pub health_reset_interval: Duration,
}

impl Default for ContainerStatusCheckerConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(30),
            query_timeout: Duration::from_secs(5),
            failure_threshold: 3,
            skip_duration: Duration::from_secs(5 * 60),
            health_reset_interval: Duration::from_secs(30 * 60),
        }
    }
}

/// 容器状态检查器
#[derive(Clone)]
struct ContainerStatusChecker {
    config: ContainerStatusCheckerConfig,
    state: Arc<AppState>,
    /// 容器健康状态映射 (lookup_key -> health_state)
    health_states: Arc<DashMap<String, ContainerHealthState>>,
}

impl ContainerStatusChecker {
    /// 创建新的状态检查器
    fn new(config: ContainerStatusCheckerConfig, state: Arc<AppState>) -> Self {
        Self {
            config,
            state,
            health_states: Arc::new(DashMap::new()),
        }
    }

    /// 检查所有容器的状态
    ///
    /// 🚀 使用并发批处理优化：每批最多 10 个容器并发检查
    /// 100 个容器从最差 300s (5分钟) 降低到 ~30s
    async fn check_all_containers(&self) -> anyhow::Result<()> {
        use futures_util::future::join_all;

        // 收集所有需要检查的容器（创建快照）
        let containers: Vec<(String, Arc<shared_types::ProjectAndContainerInfo>)> =
            self.state.projects.iter();

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

        // 🚀 预过滤：收集需要检查的容器（should_skip_check 是同步操作）
        let to_check: Vec<_> = containers
            .into_iter()
            .filter_map(|(_project_id, container_info)| {
                let lookup_key = container_info.container_key().to_string();
                if self.should_skip_check(&lookup_key) {
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
    /// 返回是否更新了活动时间
    async fn check_single_container(
        &self,
        lookup_key: &str,
        container_info: &Arc<shared_types::ProjectAndContainerInfo>,
    ) -> anyhow::Result<bool> {
        // 获取容器信息
        let container = match container_info.container_info() {
            Some(c) => c,
            None => {
                debug!("[STATUS_CHECKER] Container info not found: {}", lookup_key);
                return Ok(false);
            }
        };

        // 获取最后激活时间用于日志显示
        let last_activity = container_info.last_activity();
        let last_activity_str = format_datetime(last_activity);
        let relative_time_str = format_relative_time(last_activity);

        // 构建 gRPC 地址
        let grpc_addr = format!(
            "{}:{}",
            container.container_ip,
            shared_types::GRPC_DEFAULT_PORT
        );

        // 提取 user_id（lookup_key 可能是 user_id 或 project_id）
        let user_id = container_info
            .user_id()
            .map(|s| s.to_string())
            .unwrap_or_else(|| lookup_key.to_string());

        let project_id = container_info.project_id().to_string();

        // 查询容器状态
        match query_container_status(
            &grpc_addr,
            &user_id,
            &project_id,
            &self.state.grpc_pool,
            &self.config,
            last_activity_str,
            relative_time_str,
        )
        .await
        {
            Ok(is_active) => {
                // ✅ 成功：重置失败计数器
                self.record_success(lookup_key);

                if is_active {
                    // 容器有活跃任务，更新活动时间和状态
                    // 注意：使用 project_id 更新存储，而不是 lookup_key
                    if let Err(e) = update_project_activity(&project_id, &self.state).await {
                        warn!(
                            "[STATUS_CHECKER] Failed to update activity time: project_id={}, {}",
                            project_id, e
                        );
                        return Ok(false);
                    }
                    // 同步更新 agent 状态为 Active
                    self.state.projects.update_agent_status(
                        &project_id,
                        1, // Active
                        "active",
                    );
                    debug!(
                        "[STATUS_CHECKER] Container is active, updated activity time and status: container_key={}, project_id={}",
                        lookup_key, project_id
                    );
                    Ok(true)
                } else {
                    // 同步更新 agent 状态为 Idle
                    self.state.projects.update_agent_status(
                        &project_id,
                        0, // Idle
                        "idle",
                    );
                    debug!(
                        "[STATUS_CHECKER] Container is idle, updated status to Idle: container_key={}, project_id={}",
                        lookup_key, project_id
                    );
                    Ok(false)
                }
            }
            Err(e) => {
                // ❌ 失败：主动查询 Docker 容器是否存在（关键优化）
                let container_exists = self
                    .check_container_exists(container_info, &grpc_addr)
                    .await;

                if !container_exists {
                    // 容器不存在：清理健康状态并计入失败（进入 skip 名单）。
                    // 此前直接 return 不计数 → 消失容器的 project 每 30s 轮询都重查
                    // （网络往返 + agent_runner 查询），历史残留 project 随时间线性
                    // 放大轮询负载（compose 测试环境实测：20+ 死条目 × 2 轮/分钟）。
                    // 计入失败后 skip_duration（默认 5 分钟）内跳过，容器若被
                    // pod_ensure 重建，skip 窗口过后自然恢复检查。
                    // 项目记录本身仍由清理任务（闲置回收，默认 10 分钟超时）统一移除。
                    info!(
                        "[STATUS_CHECKER] Container has been destroyed, cleaning up health state: {}",
                        lookup_key
                    );
                    self.health_states.remove(lookup_key);
                    self.state.grpc_pool.remove(&grpc_addr).await;
                    self.record_failure(lookup_key, &grpc_addr, &e);
                    return Err(e);
                }

                // 容器存在但连接失败，记录失败（可能是网络问题）
                self.record_failure(lookup_key, &grpc_addr, &e);
                Err(e)
            }
        }
    }

    /// 检查 runtime 容器是否存在
    async fn check_container_exists(
        &self,
        container_info: &Arc<shared_types::ProjectAndContainerInfo>,
        grpc_addr: &str,
    ) -> bool {
        let runtime = self.state.runtime();
        let service_type = container_info
            .service_type()
            .unwrap_or(shared_types::ServiceType::ComputerAgentRunner);

        // 根据 service_type 使用不同的查找方法
        // - RCoder 模式：使用 project_id 查找
        // - ComputerAgentRunner 模式：使用 user_id 查找
        let exists = match service_type {
            shared_types::ServiceType::ComputerAgentRunner => {
                // ComputerAgentRunner 模式：使用 user_id 查找容器
                if let Some(user_id) = container_info.user_id() {
                    match runtime.find_container(user_id, &service_type).await {
                        Ok(Some(_)) => true,
                        Ok(None) => false,
                        Err(e) => {
                            debug!("[STATUS_CHECKER] Failed to query container: {}", e);
                            false
                        }
                    }
                } else {
                    debug!("[STATUS_CHECKER] ComputerAgentRunner missing user_id");
                    false
                }
            }
            // UserappBuilder 是 agent-runner(复用 dev-rcoder-agent-runner,有 gRPC),
            // 复用 WebAgentRunner 的 project_id 查找路径
            shared_types::ServiceType::WebAgentRunner
            | shared_types::ServiceType::UserappBuilder => {
                // RCoder / UserappBuilder 模式：使用 project_id(app_id)查找容器
                match runtime
                    .find_container(container_info.project_id(), &service_type)
                    .await
                {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(e) => {
                        debug!("[STATUS_CHECKER] Failed to query container: {}", e);
                        false
                    }
                }
            }
            // Userapp 不参与 agent 健康检查（由 app_manager 独立管理），视为不存在
            shared_types::ServiceType::Userapp => false,
        };

        if exists {
            debug!(
                "[STATUS_CHECKER] Runtime container exists, likely network issue: {} (service_type={:?})",
                grpc_addr, service_type
            );
        } else {
            info!(
                "[STATUS_CHECKER] Runtime container does not exist (already destroyed): {} (service_type={:?})",
                grpc_addr, service_type
            );
        }

        exists
    }

    /// 判断是否应该跳过检查（view: 闭包结束读锁立即释放）
    fn should_skip_check(&self, lookup_key: &str) -> bool {
        let now = Utc::now();
        self.health_states
            .view(lookup_key, |_, health| {
                if health.consecutive_failures >= self.config.failure_threshold
                    && let Some(first_failure) = health.first_failure_time
                    && let Ok(skip_duration) = chrono::Duration::from_std(self.config.skip_duration)
                {
                    return now.signed_duration_since(first_failure) < skip_duration;
                }
                false
            })
            .unwrap_or(false)
    }

    /// 记录成功并重置失败计数器
    fn record_success(&self, lookup_key: &str) {
        let now = Utc::now();

        use dashmap::mapref::entry::Entry;

        match self.health_states.entry(lookup_key.to_string()) {
            Entry::Occupied(mut entry) => {
                // 使用 get_mut 直接修改，避免克隆
                let was_failing = entry.get().consecutive_failures > 0;
                let health = entry.get_mut();
                health.consecutive_failures = 0;
                health.first_failure_time = None;
                health.last_check_time = now;
                health.last_success_time = Some(now);
                // 无需 insert，修改已生效

                if was_failing {
                    info!("[STATUS_CHECKER] Container recovered: {}", lookup_key);
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(ContainerHealthState::new());
            }
        }
    }

    /// 记录失败并清理连接
    fn record_failure(&self, lookup_key: &str, grpc_addr: &str, error: &anyhow::Error) {
        let now = Utc::now();

        use dashmap::mapref::entry::Entry;

        let consecutive_failures = match self.health_states.entry(lookup_key.to_string()) {
            Entry::Occupied(mut entry) => {
                // 使用 get_mut 直接修改，避免克隆
                let health = entry.get_mut();
                health.consecutive_failures += 1;
                health.last_check_time = now;
                if health.first_failure_time.is_none() {
                    health.first_failure_time = Some(now);
                }
                // 无需 insert，修改已生效
                health.consecutive_failures
            }
            Entry::Vacant(entry) => {
                entry.insert(ContainerHealthState::new_failed());
                1
            }
        };

        // 🔌 第1次失败或达到阈值时，清理 gRPC 连接池
        // 用 spawn fire-and-forget：status_checker 是周期性任务，下次循环会重新检查，
        // 不需要 remove 立即生效（与 chat 重试路径不同）
        if consecutive_failures == 1 || consecutive_failures == self.config.failure_threshold {
            let pool = self.state.grpc_pool.clone();
            let addr_owned = grpc_addr.to_string();
            tokio::spawn(async move {
                pool.remove(&addr_owned).await;
            });
            info!("[STATUS_CHECKER] Already cleanup connection: {}", grpc_addr);
        }

        // 📊 分级日志输出
        match consecutive_failures {
            1 => {
                // 首次失败：INFO 级别
                info!(
                    "[STATUS_CHECKER] Container first query failed: {} - {}",
                    lookup_key, error
                );
            }
            n if n < self.config.failure_threshold => {
                // 持续失败但未达到阈值：DEBUG 级别
                debug!(
                    "[STATUS_CHECKER] Container continuous failure ({}/{}): {}",
                    n, self.config.failure_threshold, lookup_key
                );
            }
            n if n == self.config.failure_threshold => {
                // 达到阈值：WARN 级别
                warn!(
                    "[STATUS_CHECKER] Container continuous failures reached threshold, will skip check temporarily: {} (failures: {})",
                    lookup_key, n
                );
            }
            _ => {
                // 超过阈值后的偶发检查：DEBUG 级别
                debug!("[STATUS_CHECKER] Skipping check for: {}", lookup_key);
            }
        }
    }

    /// 清理过期的健康状态
    fn cleanup_stale_health_states(&self) {
        let now = Utc::now();
        let retention_duration = match chrono::Duration::from_std(self.config.health_reset_interval)
        {
            Ok(d) => d,
            Err(_) => return,
        };

        let mut removed_count = 0;

        // 第一步：从 health_states 收集数据（iter 读锁在 collect 后释放）
        let candidates: Vec<(String, bool)> = self
            .health_states
            .iter()
            .map(|entry| {
                let lookup_key = entry.key().clone();
                let elapsed = now.signed_duration_since(entry.value().last_check_time);
                let is_stale = elapsed > retention_duration;
                (lookup_key, is_stale)
            })
            .collect();

        // 第二步：跨 map 查询（health_states 锁已释放，无死锁风险）
        let keys_to_remove: Vec<String> = candidates
            .into_iter()
            .filter(|(lookup_key, is_stale)| {
                let not_in_storage = !self.state.contains_project(lookup_key);
                not_in_storage || *is_stale
            })
            .map(|(k, _)| k)
            .collect();

        // 第三步：批量移除
        for key in keys_to_remove {
            if self.health_states.remove(&key).is_some() {
                removed_count += 1;
                debug!("[STATUS_CHECKER] Cleaned up stale health state: {}", key);
            }
        }

        if removed_count > 0 {
            info!(
                "[STATUS_CHECKER] Cleaned up stale health states: removed={}",
                removed_count
            );
        }
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
                    if let Err(e) = checker
                        .check_all_containers()
                        .instrument(tracing::info_span!("bg_status_check"))
                        .await
                    {
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

/// 查询容器状态
///
/// 返回容器是否活跃（有活跃任务）
async fn query_container_status(
    grpc_addr: &str,
    user_id: &str,
    project_id: &str,
    grpc_pool: &Arc<GrpcChannelPool>,
    config: &ContainerStatusCheckerConfig,
    last_activity_str: String,
    relative_time_str: String,
) -> anyhow::Result<bool> {
    // 获取 gRPC 客户端
    let mut client = grpc_pool.get_client(grpc_addr).await?;

    // 构建请求
    let request = tonic::Request::new(GetContainerStatusRequest {
        user_id: user_id.to_string(),
        project_id: project_id.to_string(),
    });

    // 发送请求（带超时）
    let response =
        time::timeout(config.query_timeout, client.get_container_status(request)).await??;

    let status_response = response.into_inner();

    debug!(
        "[STATUS_CHECKER] Container status: user_id={}, is_active={}, active_tasks={}, status={}, last_activity={} ({})",
        user_id,
        status_response.is_active,
        status_response.active_tasks,
        status_response.status,
        last_activity_str,
        relative_time_str
    );

    // 如果容器有活跃任务，则认为容器活跃
    Ok(status_response.is_active || status_response.active_tasks > 0)
}

/// 更新项目活动时间（并同步更新关联容器的活动时间）
///
/// 更新项目的 last_activity 字段
async fn update_project_activity(project_id: &str, state: &Arc<AppState>) -> anyhow::Result<()> {
    // 使用 ProjectAdapter 的 update_activity 方法
    // 该方法会同时更新 project 和关联 container 的 last_activity
    state.update_activity(project_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 状态检查器状态机回归网（历史上 4 次误杀事故的防线，此前 0 测试）。
    //! 锁住的偏移敏感点：
    //! - 失败计数升降级与 first_failure_time 只钉首次（窗口锚点漂移 = skip 永期推迟）
    //! - skip 窗口的阈值与时长双条件
    //! - check_container_exists 按 service_type 的查找键分派（分派轴改错 = 清理错容器）
    //! - 健康状态的清理双轴（不在存储 / 超期）

    use super::*;
    use crate::config::AppConfig;
    use crate::grpc::SessionStreamRegistry;
    use crate::router::AppState;
    use crate::storage::{ProjectAdapter, ProjectStoreBackend};
    use agent_provisioning::AgentDownloadManager;
    use app_manager::config::{AppAccessMode, AppManagerConfig};
    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use container_runtime_api::{
        AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
        ContainerRuntimeStatus, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
    };
    use dashmap::DashMap;
    use shared_types::{
        ApiKeyAuthConfig, ContainerBasicInfo, ProjectAndContainerInfo, ProjectExtendedFields,
        ServiceType,
    };
    use std::sync::Mutex;
    use tokio::sync::broadcast;

    /// find_container 的可重复响应行为（多次查询返回一致结果）
    #[derive(Clone, Copy)]
    enum FindBehavior {
        Found,
        Missing,
        Fail,
    }

    /// 可编程桩运行时：记录 find_container 的每次查询参数，返回行为可由测试预设。
    /// 分派语义测试的核心——验证查找键用的是 user_id 还是 project_id。
    struct ProbeRuntime {
        behavior: FindBehavior,
        queries: Mutex<Vec<(String, ServiceType)>>,
    }

    impl ProbeRuntime {
        fn new(behavior: FindBehavior) -> Self {
            Self {
                behavior,
                queries: Mutex::new(Vec::new()),
            }
        }

        fn queried_identifiers(&self) -> Vec<String> {
            self.queries
                .lock()
                .unwrap()
                .iter()
                .map(|(id, _)| id.clone())
                .collect()
        }
    }

    #[async_trait]
    impl AgentContainerRuntime for ProbeRuntime {
        async fn create_container(
            &self,
            _params: ContainerCreateParams,
        ) -> ContainerRuntimeResult<ContainerBasicInfo> {
            Err(ContainerRuntimeError::ContainerNotFound("probe".into()))
        }
        async fn get_container_info(
            &self,
            _project_id: &str,
        ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
            Ok(None)
        }
        async fn find_container(
            &self,
            identifier: &str,
            service_type: &ServiceType,
        ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
            self.queries
                .lock()
                .unwrap()
                .push((identifier.to_string(), service_type.clone()));
            match self.behavior {
                FindBehavior::Found => Ok(Some(RuntimeContainerInfo {
                    container_id: "stub".to_string(),
                    container_name: "stub".to_string(),
                    container_ip: "127.0.0.1".to_string(),
                    status: ContainerRuntimeStatus::Running,
                    created_at: Utc::now(),
                    env_vars: None,
                })),
                FindBehavior::Missing => Ok(None),
                FindBehavior::Fail => Err(ContainerRuntimeError::ConnectionError(
                    "probe error".to_string(),
                )),
            }
        }
        async fn stop_container(&self, _project_id: &str) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn is_container_running(&self, _project_id: &str) -> ContainerRuntimeResult<bool> {
            Ok(false)
        }
        async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
            Ok(vec![])
        }
        async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn health_check(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
    }

    // 空 impl 继承默认实现 → ProbeRuntime impl B/C → 自动 impl ContainerRuntime
    // 例外：list_deployments 必须返回空列表——AppService::new 构造期会调它
    // （rebuild_stopped_apps），默认实现的"not supported"会让构造直接失败
    #[async_trait]
    impl WorkspaceRuntime for ProbeRuntime {}
    #[async_trait]
    impl UserAppDeploymentRuntime for ProbeRuntime {
        async fn list_deployments(
            &self,
        ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
            Ok(vec![])
        }
    }

    /// 轻量 AppState 字面量构造（绕过 AppState::new 的 AppService 装配副作用）。
    /// 状态检查器只消费 runtime / grpc_pool / projects 三个字段。
    async fn test_state(runtime: Arc<ProbeRuntime>) -> Arc<AppState> {
        let (adapter, _cleanup_rx) = ProjectAdapter::new("test-ns".to_string(), "cluster.local".to_string());
        let activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(300)));
        // 显式 Docker 模式：AppService::new 的 K8s 分支会调 validate_app_prerequisites
        let manager_config = AppManagerConfig {
            access_mode: AppAccessMode::Docker,
            ..AppManagerConfig::default()
        };
        let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
            app_manager::service::AppService::new(
                manager_config,
                runtime.clone(),
                activity.clone(),
                None,
            )
            .await
            .expect("AppService 构造失败"),
        );
        let download_dir = tempfile::tempdir().expect("tempdir");
        let agent_download_manager = Arc::new(
            AgentDownloadManager::new(download_dir.path()).expect("下载管理器构造失败"),
        );
        let (pod_created_tx, _) = broadcast::channel(32);
        Arc::new(AppState {
            config: AppConfig::default(),
            projects: Arc::new(ProjectStoreBackend::Memory(Arc::new(adapter))),
            pingora_service: None,
            grpc_pool: Arc::new(GrpcChannelPool::new()),
            session_stream_registry: Arc::new(SessionStreamRegistry::new()),
            api_key_config: Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig::default())),
            pod_creating: Arc::new(DashMap::new()),
            pod_created_tx: Arc::new(pod_created_tx),
            container_prefix_rcoder: "dev-rcoder".to_string(),
            container_prefix_computer: "computer-agent-runner".to_string(),
            runtime,
            cleanup_rx: Arc::new(Mutex::new(None)),
            agent_download_manager,
            app_service,
            activity,
            cluster_domain: "cluster.local".to_string(),
        })
    }

    fn checker(config: ContainerStatusCheckerConfig, state: Arc<AppState>) -> ContainerStatusChecker {
        ContainerStatusChecker::new(config, state)
    }

    /// 测试载体：可控 service_type / user_id 的容器信息
    fn container_info(project_id: &str, user_id: Option<&str>, service_type: ServiceType) -> Arc<ProjectAndContainerInfo> {
        let container = ContainerBasicInfo {
            container_id: format!("container_{project_id}"),
            container_name: format!("container_{project_id}"),
            container_ip: "127.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: project_id.to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: format!("http://{project_id}"),
        };
        Arc::new(ProjectAndContainerInfo::from_parts(
            project_id.to_string(),
            user_id.map(str::to_string),
            None,
            None,
            Some(container),
            ProjectExtendedFields {
                service_type: Some(service_type),
                ..Default::default()
            },
        ))
    }

    #[tokio::test]
    async fn record_failure_escalates_and_pins_first_failure_time() {
        let state = test_state(Arc::new(ProbeRuntime::new(FindBehavior::Missing))).await;
        let c = checker(ContainerStatusCheckerConfig::default(), state);

        c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
        c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
        c.record_failure("k", "addr", &anyhow::anyhow!("boom"));

        let health = c.health_states.get("k").expect("状态条目存在").clone();
        assert_eq!(health.consecutive_failures, 3, "连续失败应累计到阈值");
        let pinned = health
            .first_failure_time
            .expect("首次失败时间应被记录");
        // 再失败一次：first_failure_time 不得漂移（skip 窗口锚定首次失败，
        // 若改为每次更新，容器会因间歇失败被永久跳过检查）
        std::thread::sleep(Duration::from_millis(5));
        c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
        let health = c.health_states.get("k").expect("状态条目存在").clone();
        assert_eq!(health.consecutive_failures, 4);
        assert_eq!(
            health.first_failure_time.expect("仍有首次时间"),
            pinned,
            "first_failure_time 必须钉在首次失败"
        );
    }

    #[tokio::test]
    async fn record_success_resets_failure_state_completely() {
        let state = test_state(Arc::new(ProbeRuntime::new(FindBehavior::Missing))).await;
        let c = checker(ContainerStatusCheckerConfig::default(), state);

        c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
        c.record_failure("k", "addr", &anyhow::anyhow!("boom"));
        c.record_success("k");

        let health = c.health_states.get("k").expect("状态条目存在").clone();
        assert_eq!(health.consecutive_failures, 0, "成功必须清零失败计数");
        assert!(
            health.first_failure_time.is_none(),
            "成功必须清掉 first_failure_time——否则恢复后一次失败即重新进入 skip 窗口语义"
        );
        assert!(health.last_success_time.is_some());
    }

    #[tokio::test]
    async fn should_skip_requires_threshold_and_unexpired_window() {
        let state = test_state(Arc::new(ProbeRuntime::new(FindBehavior::Missing))).await;
        let mut config = ContainerStatusCheckerConfig::default();
        config.failure_threshold = 3;
        config.skip_duration = Duration::from_secs(300);
        let c = checker(config, state);

        // 未达阈值：不跳过
        c.health_states.insert(
            "below".to_string(),
            ContainerHealthState {
                consecutive_failures: 2,
                first_failure_time: Some(Utc::now()),
                last_check_time: Utc::now(),
                last_success_time: None,
            },
        );
        assert!(!c.should_skip_check("below"), "未达阈值不应跳过");

        // 达阈值且在窗口内：跳过
        c.health_states.insert(
            "inside".to_string(),
            ContainerHealthState {
                consecutive_failures: 3,
                first_failure_time: Some(Utc::now() - chrono::Duration::seconds(60)),
                last_check_time: Utc::now(),
                last_success_time: None,
            },
        );
        assert!(c.should_skip_check("inside"), "达阈值且窗口内应跳过");

        // 窗口已过：恢复检查（容器若被重建，正是靠这条路径自愈）
        c.health_states.insert(
            "expired".to_string(),
            ContainerHealthState {
                consecutive_failures: 5,
                first_failure_time: Some(Utc::now() - chrono::Duration::seconds(301)),
                last_check_time: Utc::now(),
                last_success_time: None,
            },
        );
        assert!(!c.should_skip_check("expired"), "窗口过期必须恢复检查");

        // 无条目：不跳过
        assert!(!c.should_skip_check("absent"));
    }

    #[tokio::test]
    async fn check_container_exists_dispatches_lookup_key_by_service_type() {
        let probe = Arc::new(ProbeRuntime::new(FindBehavior::Found));
        let state = test_state(probe.clone()).await;
        let c = checker(ContainerStatusCheckerConfig::default(), state);

        // ComputerAgentRunner：用 user_id 查
        let info = container_info("proj-1", Some("user-1"), ServiceType::ComputerAgentRunner);
        assert!(c.check_container_exists(&info, "addr").await);
        assert_eq!(
            probe.queried_identifiers(),
            vec!["user-1".to_string()],
            "ComputerAgentRunner 必须以 user_id 为查找键"
        );

        // WebAgentRunner：用 project_id 查
        let info = container_info("proj-2", Some("user-2"), ServiceType::WebAgentRunner);
        assert!(c.check_container_exists(&info, "addr").await);
        assert_eq!(
            probe.queried_identifiers().last().unwrap(),
            "proj-2",
            "WebAgentRunner 必须以 project_id 为查找键"
        );

        // UserappBuilder：复用 project_id 路径
        let info = container_info("proj-3", Some("user-3"), ServiceType::UserappBuilder);
        assert!(c.check_container_exists(&info, "addr").await);
        assert_eq!(
            probe.queried_identifiers().last().unwrap(),
            "proj-3",
            "UserappBuilder 必须以 project_id 为查找键"
        );

        // Userapp：不查 runtime，恒视为不存在（由 app_manager 独立管理）
        let before = probe.queried_identifiers().len();
        let info = container_info("proj-4", Some("user-4"), ServiceType::Userapp);
        assert!(
            !c.check_container_exists(&info, "addr").await,
            "Userapp 恒视为不存在"
        );
        assert_eq!(
            probe.queried_identifiers().len(),
            before,
            "Userapp 分支不得触碰 runtime 查询"
        );
    }

    #[tokio::test]
    async fn check_container_exists_treats_error_and_missing_user_id_as_absent() {
        // 查询 Err：保守视为不存在（触发连接清理路径）
        let state = test_state(Arc::new(ProbeRuntime::new(FindBehavior::Fail))).await;
        let c = checker(ContainerStatusCheckerConfig::default(), state);
        let info = container_info("proj", Some("user"), ServiceType::ComputerAgentRunner);
        assert!(!c.check_container_exists(&info, "addr").await);

        // ComputerAgentRunner 缺 user_id：视为不存在且不查
        let probe = Arc::new(ProbeRuntime::new(FindBehavior::Found));
        let state = test_state(probe.clone()).await;
        let c = checker(ContainerStatusCheckerConfig::default(), state);
        let info = container_info("proj", None, ServiceType::ComputerAgentRunner);
        assert!(!c.check_container_exists(&info, "addr").await);
        assert!(probe.queried_identifiers().is_empty());
    }

    #[tokio::test]
    async fn cleanup_stale_health_states_removes_unknown_or_expired_only() {
        let probe = Arc::new(ProbeRuntime::new(FindBehavior::Missing));
        let state = test_state(probe).await;
        let mut config = ContainerStatusCheckerConfig::default();
        config.health_reset_interval = Duration::from_secs(1800);
        let c = checker(config, state.clone());

        // 条目 A：不在 projects 存储 → 移除
        c.health_states.insert(
            "unknown".to_string(),
            ContainerHealthState::new(),
        );
        // 条目 B：在存储且新近检查 → 保留
        state.insert_project(
            "known".to_string(),
            container_info("known", Some("u"), ServiceType::WebAgentRunner),
        );
        c.health_states.insert(
            "known".to_string(),
            ContainerHealthState::new(),
        );
        // 条目 C：在存储但 last_check 超 reset 周期 → 移除
        state.insert_project(
            "stale".to_string(),
            container_info("stale", Some("u"), ServiceType::WebAgentRunner),
        );
        c.health_states.insert(
            "stale".to_string(),
            ContainerHealthState {
                consecutive_failures: 1,
                first_failure_time: None,
                last_check_time: Utc::now() - chrono::Duration::seconds(1801),
                last_success_time: None,
            },
        );

        c.cleanup_stale_health_states();

        assert!(c.health_states.get("unknown").is_none(), "不在存储的条目应被清理");
        assert!(c.health_states.get("known").is_some(), "在存储且未过期的条目必须保留");
        assert!(c.health_states.get("stale").is_none(), "超期条目应被清理");
    }
}
