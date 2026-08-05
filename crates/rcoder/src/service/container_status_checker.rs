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
use tracing::{debug, info, warn};

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
            debug!(" [STATUS_CHECKER] No containers to check");
            return Ok(());
        }

        info!(
            " [STATUS_CHECKER] Starting to check {} containers",
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
                        " [STATUS_CHECKER] Skipping check (recently failed): {}",
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
            " [STATUS_CHECKER] Check completed: total={}, checked={}, skipped={}, updated={}, failed={}",
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
                debug!(" [STATUS_CHECKER] Container info not found: {}", lookup_key);
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
                            " [STATUS_CHECKER] Failed to update activity time: project_id={}, {}",
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
                        " [STATUS_CHECKER] Container is active, updated activity time and status: container_key={}, project_id={}",
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
                        " [STATUS_CHECKER] Container is idle, updated status to Idle: container_key={}, project_id={}",
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
                    // 容器不存在，直接清理所有状态
                    info!(
                        " [STATUS_CHECKER] Container has been destroyed, cleaning up health state: {}",
                        lookup_key
                    );
                    self.health_states.remove(lookup_key);
                    self.state.grpc_pool.remove(&grpc_addr).await;
                    // 注意：不移除存储中的项目记录，由清理任务统一处理
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
                            debug!(" [STATUS_CHECKER] Failed to query container: {}", e);
                            false
                        }
                    }
                } else {
                    debug!(" [STATUS_CHECKER] ComputerAgentRunner missing user_id");
                    false
                }
            }
            // UserAppBuilder 是 agent-runner(复用 dev-rcoder-agent-runner,有 gRPC),
            // 复用 WebAgentRunner 的 project_id 查找路径
            shared_types::ServiceType::WebAgentRunner
            | shared_types::ServiceType::UserAppBuilder => {
                // RCoder / UserAppBuilder 模式：使用 project_id(app_id)查找容器
                match runtime
                    .find_container(container_info.project_id(), &service_type)
                    .await
                {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(e) => {
                        debug!(" [STATUS_CHECKER] Failed to query container: {}", e);
                        false
                    }
                }
            }
            // UserApp 不参与 agent 健康检查（由 app_manager 独立管理），视为不存在
            shared_types::ServiceType::UserApp => false,
        };

        if exists {
            debug!(
                " [STATUS_CHECKER] Runtime container exists, likely network issue: {} (service_type={:?})",
                grpc_addr, service_type
            );
        } else {
            info!(
                " [STATUS_CHECKER] Runtime container does not exist (already destroyed): {} (service_type={:?})",
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
            info!(
                " [STATUS_CHECKER] Already cleanup connection: {}",
                grpc_addr
            );
        }

        // 📊 分级日志输出
        match consecutive_failures {
            1 => {
                // 首次失败：INFO 级别
                info!(
                    " [STATUS_CHECKER] Container first query failed: {} - {}",
                    lookup_key, error
                );
            }
            n if n < self.config.failure_threshold => {
                // 持续失败但未达到阈值：DEBUG 级别
                debug!(
                    " [STATUS_CHECKER] Container continuous failure ({}/{}): {}",
                    n, self.config.failure_threshold, lookup_key
                );
            }
            n if n == self.config.failure_threshold => {
                // 达到阈值：WARN 级别
                warn!(
                    " [STATUS_CHECKER] Container continuous failures reached threshold, will skip check temporarily: {} (failures: {})",
                    lookup_key, n
                );
            }
            _ => {
                // 超过阈值后的偶发检查：DEBUG 级别
                debug!(" [STATUS_CHECKER] Skipping check for: {}", lookup_key);
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
                debug!(" [STATUS_CHECKER] Cleaned up stale health state: {}", key);
            }
        }

        if removed_count > 0 {
            info!(
                " [STATUS_CHECKER] Cleaned up stale health states: removed={}",
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
        " [STATUS_CHECKER] Starting container status checker: interval={}s, failure_threshold={}, skip_duration={}s",
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
                    // 执行容器状态检查
                    if let Err(e) = checker.check_all_containers().await {
                        warn!(" [STATUS_CHECKER] Container status check failed: {}", e);
                    }

                    // 定期清理过期的健康状态
                    cleanup_counter += 1;
                    if cleanup_counter >= cleanup_interval {
                        checker.cleanup_stale_health_states();
                        cleanup_counter = 0;
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!(" [STATUS_CHECKER] shutdown");
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
        tokio::time::timeout(config.query_timeout, client.get_container_status(request)).await??;

    let status_response = response.into_inner();

    debug!(
        " [STATUS_CHECKER] Container status: user_id={}, is_active={}, active_tasks={}, status={}, last_activity={} ({})",
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
