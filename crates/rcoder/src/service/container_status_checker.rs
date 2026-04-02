//! 容器状态检查器
//!
//! 定期查询 Agent Runner 的容器状态，如果容器有活跃任务则更新活动时间。
//! 这样可以防止正在执行长时间任务的容器被清理任务误判为闲置而销毁。
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
    async fn check_all_containers(&self) -> anyhow::Result<()> {
        // 收集所有需要检查的容器（创建快照，使用 DuckDB 存储）
        let containers: Vec<(String, Arc<shared_types::ProjectAndContainerInfo>)> =
            self.state.projects.iter().collect();

        if containers.is_empty() {
            debug!("📭 [STATUS_CHECKER] message check message container");
            return Ok(());
        }

        info!(
            "🔍 [STATUS_CHECKER] Starting to check {} containers",
            containers.len()
        );

        let total_count = containers.len();
        let mut checked = 0;
        let mut skipped = 0;
        let mut updated = 0;
        let mut failed = 0;

        for (_project_id, container_info) in containers {
            // 使用 container_key() 获取正确的容器标识符
            // - RCoder 模式：返回 project_id
            // - ComputerAgentRunner 模式：返回 user_id（用于匹配容器名称）
            let lookup_key = container_info.container_key().to_string();

            // 🆕 检查所有类型的容器（RCoder 和 ComputerAgentRunner）
            // 两种模式都可能执行长时间任务，需要定期检查状态防止被误杀

            // 检查是否应该跳过
            if self.should_skip_check(&lookup_key) {
                skipped += 1;
                debug!(
                    "⏭️ [STATUS_CHECKER] skipcheck(failed message ): {}",
                    lookup_key
                );
                continue;
            }

            checked += 1;

            // 执行单个容器检查
            match self
                .check_single_container(&lookup_key, &container_info)
                .await
            {
                Ok(true) => updated += 1,
                Ok(false) => {} // 容器空闲或未更新
                Err(_) => failed += 1,
            }
        }

        info!(
            "📊 [STATUS_CHECKER] Check completed: total={}, checked={}, skipped={}, updated={}, failed={}",
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
        let container = match container_info.container() {
            Some(c) => c,
            None => {
                debug!("⚠️ [STATUS_CHECKER] container message : {}", lookup_key);
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
                    // 注意：使用 project_id 更新 DuckDB，而不是 lookup_key
                    if let Err(e) = update_project_activity(&project_id, &self.state).await {
                        warn!(
                            "⚠️ [STATUS_CHECKER] Failed to update activity time: project_id={}, {}",
                            project_id, e
                        );
                        return Ok(false);
                    }
                    // 🆕 同步更新 agent 状态为 Active
                    if let Err(e) = self.state.projects.update_agent_status(
                        &project_id,
                        1, // Active
                        "active",
                    ) {
                        warn!(
                            "⚠️ [STATUS_CHECKER] 更新 agent 状态为 Active 失败: project_id={}, error={}",
                            project_id, e
                        );
                    }
                    debug!(
                        "✅ [STATUS_CHECKER] 容器活跃，已更新活动时间和状态: container_key={}, project_id={}",
                        lookup_key, project_id
                    );
                    Ok(true)
                } else {
                    // 🆕 同步更新 agent 状态为 Idle
                    if let Err(e) = self.state.projects.update_agent_status(
                        &project_id,
                        0, // Idle
                        "idle",
                    ) {
                        warn!(
                            "⚠️ [STATUS_CHECKER] 更新 agent 状态为 Idle 失败: project_id={}, error={}",
                            project_id, e
                        );
                    }
                    debug!(
                        "📭 [STATUS_CHECKER] 容器空闲，已更新状态为 Idle: container_key={}, project_id={}",
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
                        "🗑️ [STATUS_CHECKER] 容器已被销毁，清理健康状态: {}",
                        lookup_key
                    );
                    self.health_states.remove(lookup_key);
                    self.state.grpc_pool.remove(&grpc_addr);
                    // 注意：不移除 DuckDB 存储中的项目记录，由清理任务统一处理
                    return Err(e);
                }

                // 容器存在但连接失败，记录失败（可能是网络问题）
                self.record_failure(lookup_key, &grpc_addr, &e);
                Err(e)
            }
        }
    }

    /// 检查 Docker 容器是否存在
    async fn check_container_exists(
        &self,
        container_info: &Arc<shared_types::ProjectAndContainerInfo>,
        grpc_addr: &str,
    ) -> bool {
        match docker_manager::global::get_global_docker_manager().await {
            Ok(docker_manager) => {
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
                            match docker_manager
                                .find_user_container(user_id, &service_type)
                                .await
                            {
                                Ok(Some(_)) => true,
                                Ok(None) => false,
                                Err(e) => {
                                    debug!("⚠️ [STATUS_CHECKER] Failed to query container: {}", e);
                                    false
                                }
                            }
                        } else {
                            debug!("⚠️ [STATUS_CHECKER] ComputerAgentRunner message user_id");
                            false
                        }
                    }
                    shared_types::ServiceType::RCoder => {
                        // RCoder 模式：使用 project_id 查找容器
                        match docker_manager
                            .find_project_container(container_info.project_id(), &service_type)
                            .await
                        {
                            Ok(Some(_)) => true,
                            Ok(None) => false,
                            Err(e) => {
                                debug!("⚠️ [STATUS_CHECKER] Failed to query container: {}", e);
                                false
                            }
                        }
                    }
                };

                if exists {
                    debug!(
                        "🔍 [STATUS_CHECKER] Docker 容器存在，可能是网络问题: {} (service_type={:?})",
                        grpc_addr, service_type
                    );
                } else {
                    info!(
                        "🔍 [STATUS_CHECKER] Docker 容器不存在（已被销毁）: {} (service_type={:?})",
                        grpc_addr, service_type
                    );
                }

                exists
            }
            Err(e) => {
                warn!("[STATUS_CHECKER] get Docker Manager failed: {}", e);
                // 无法确定容器状态，保守地认为容器存在
                true
            }
        }
    }

    /// 判断是否应该跳过检查
    fn should_skip_check(&self, lookup_key: &str) -> bool {
        if let Some(health) = self.health_states.get(lookup_key) {
            let now = Utc::now();

            // 如果连续失败次数超过阈值
            if health.consecutive_failures >= self.config.failure_threshold {
                // 检查是否还在跳过期内
                if let Some(first_failure) = health.first_failure_time {
                    let elapsed = now.signed_duration_since(first_failure);
                    if let Ok(skip_duration) = chrono::Duration::from_std(self.config.skip_duration)
                    {
                        if elapsed < skip_duration {
                            return true; // 仍在跳过期内
                        }
                    }
                }
                // 跳过期已过，允许重新检查（但不重置失败计数器）
            }
        }

        false // 默认不跳过
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
                    info!("[STATUS_CHECKER] container message : {}", lookup_key);
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
                let failures = health.consecutive_failures;
                // 无需 insert，修改已生效
                failures
            }
            Entry::Vacant(entry) => {
                entry.insert(ContainerHealthState::new_failed());
                1
            }
        };

        // 🔌 第1次失败或达到阈值时，清理 gRPC 连接池
        if consecutive_failures == 1 || consecutive_failures == self.config.failure_threshold {
            self.state.grpc_pool.remove(grpc_addr);
            info!(
                "🔌 [STATUS_CHECKER] alreadycleanup message connection: {}",
                grpc_addr
            );
        }

        // 📊 分级日志输出
        match consecutive_failures {
            1 => {
                // 首次失败：INFO 级别
                info!(
                    "❌ [STATUS_CHECKER] 容器首次Query failed: {} - {}",
                    lookup_key, error
                );
            }
            n if n < self.config.failure_threshold => {
                // 持续失败但未达到阈值：DEBUG 级别
                debug!(
                    "❌ [STATUS_CHECKER] 容器持续失败 ({}/{}): {}",
                    n, self.config.failure_threshold, lookup_key
                );
            }
            n if n == self.config.failure_threshold => {
                // 达到阈值：WARN 级别
                warn!(
                    "⚠️ [STATUS_CHECKER] 容器连续失败达到阈值，将暂时跳过检查: {} (失败次数: {})",
                    lookup_key, n
                );
            }
            _ => {
                // 超过阈值后的偶发检查：DEBUG 级别
                debug!("⏭️ [STATUS_CHECKER] container message : {}", lookup_key);
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

        // 收集需要移除的 key
        let keys_to_remove: Vec<String> = self
            .health_states
            .iter()
            .filter_map(|entry| {
                let lookup_key = entry.key();
                let health = entry.value();

                // 移除条件：
                // 1. 容器已不在 DuckDB 存储中
                // 2. 最后检查时间超过健康重置周期
                let not_in_storage = !self.state.contains_project(lookup_key);
                let elapsed = now.signed_duration_since(health.last_check_time);
                let is_stale = elapsed > retention_duration;

                if not_in_storage || is_stale {
                    Some(lookup_key.clone())
                } else {
                    None
                }
            })
            .collect();

        // 批量移除
        for key in keys_to_remove {
            if self.health_states.remove(&key).is_some() {
                removed_count += 1;
                debug!("🧹 [STATUS_CHECKER] alreadycleanup message status: {}", key);
            }
        }

        if removed_count > 0 {
            info!(
                "🧹 [STATUS_CHECKER] 清理过期健康状态: 移除数量={}",
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
) -> tokio::task::JoinHandle<()> {
    info!(
        "🔍 [STATUS_CHECKER] 启动容器状态检查任务: 间隔={}秒, 失败阈值={}, 跳过时间={}秒",
        config.check_interval.as_secs(),
        config.failure_threshold,
        config.skip_duration.as_secs()
    );

    let checker = Arc::new(ContainerStatusChecker::new(config.clone(), state));

    tokio::spawn(async move {
        let mut interval = time::interval(config.check_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let mut cleanup_counter = 0;
        let cleanup_interval = 10; // 每 10 次检查清理一次健康状态

        loop {
            interval.tick().await;

            // 执行容器状态检查
            if let Err(e) = checker.check_all_containers().await {
                warn!("[STATUS_CHECKER] containerstatuscheckfailed: {}", e);
            }

            // 定期清理过期的健康状态
            cleanup_counter += 1;
            if cleanup_counter >= cleanup_interval {
                checker.cleanup_stale_health_states();
                cleanup_counter = 0;
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
        "📊 [STATUS_CHECKER] 容器状态: user_id={}, is_active={}, active_tasks={}, status={}, last_activity={} ({})",
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
/// 使用 DuckDB 存储更新 projects 表的 last_activity 字段
async fn update_project_activity(project_id: &str, state: &Arc<AppState>) -> anyhow::Result<()> {
    // 使用 ProjectAdapter 的 update_activity 方法
    // 该方法会同时更新 project 和关联 container 的 last_activity
    state.update_activity(project_id);
    Ok(())
}
