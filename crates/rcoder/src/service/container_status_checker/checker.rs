//! ContainerStatusChecker 主体：周期检查流程、容器存在性分派、启动入口。

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::time;
use tracing::{Instrument, debug, info, warn};

use crate::router::AppState;
use shared_types::ProjectStore as _;

use super::state::{ContainerHealthState, ContainerStatusCheckerConfig};

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

/// 容器状态检查器
#[derive(Clone)]
pub(super) struct ContainerStatusChecker {
    pub(super) config: ContainerStatusCheckerConfig,
    pub(super) state: Arc<AppState>,
    /// 容器健康状态映射 (lookup_key -> health_state)
    pub(super) health_states: Arc<DashMap<String, ContainerHealthState>>,
}

impl ContainerStatusChecker {
    /// 创建新的状态检查器
    pub(super) fn new(config: ContainerStatusCheckerConfig, state: Arc<AppState>) -> Self {
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
    pub(super) async fn check_container_exists(
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

/// 更新项目活动时间（并同步更新关联容器的活动时间）
///
/// 更新项目的 last_activity 字段
async fn update_project_activity(project_id: &str, state: &Arc<AppState>) -> anyhow::Result<()> {
    // 使用 ProjectAdapter 的 update_activity 方法
    // 该方法会同时更新 project 和关联 container 的 last_activity
    state.update_activity(project_id);
    Ok(())
}
