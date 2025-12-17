//! 容器状态检查器
//!
//! 定期查询 Agent Runner 的容器状态，如果容器有活跃任务则更新活动时间。
//! 这样可以防止正在执行长时间任务的容器被清理任务误判为闲置而销毁。

use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{debug, info, warn};

use crate::grpc::GrpcChannelPool;
use crate::router::AppState;
use shared_types::grpc::GetContainerStatusRequest;

/// 容器状态检查配置
#[derive(Debug, Clone)]
pub struct ContainerStatusCheckerConfig {
    /// 检查间隔（默认 30 秒）
    pub check_interval: Duration,
    /// 查询超时（默认 5 秒）
    pub query_timeout: Duration,
}

impl Default for ContainerStatusCheckerConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(30),
            query_timeout: Duration::from_secs(5),
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
        "🔍 [STATUS_CHECKER] 启动容器状态检查任务: 间隔={}秒",
        config.check_interval.as_secs()
    );

    tokio::spawn(async move {
        let mut interval = time::interval(config.check_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            if let Err(e) = check_all_containers(&config, &state).await {
                warn!("⚠️ [STATUS_CHECKER] 容器状态检查失败: {}", e);
            }
        }
    })
}

/// 检查所有容器的状态
async fn check_all_containers(
    config: &ContainerStatusCheckerConfig,
    state: &Arc<AppState>,
) -> anyhow::Result<()> {
    // 收集所有需要检查的容器
    let containers: Vec<(String, Arc<shared_types::ProjectAndContainerInfo>)> = state
        .project_and_agent_map
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect();

    if containers.is_empty() {
        debug!("📭 [STATUS_CHECKER] 没有需要检查的容器");
        return Ok(());
    }

    info!(
        "🔍 [STATUS_CHECKER] 开始检查 {} 个容器状态",
        containers.len()
    );

    let total_count = containers.len();
    let mut checked = 0;
    let mut updated = 0;
    let mut failed = 0;

    for (lookup_key, container_info) in containers {
        // 只检查 ComputerAgentRunner 类型的容器
        // RCoder 模式的容器在收到新请求时会自动更新活动时间
        if !matches!(
            container_info.service_type(),
            Some(shared_types::ServiceType::ComputerAgentRunner)
        ) {
            continue;
        }

        checked += 1;

        // 获取容器信息
        let container = match container_info.container() {
            Some(c) => c,
            None => {
                debug!("⚠️ [STATUS_CHECKER] 容器信息缺失: {}", lookup_key);
                continue;
            }
        };

        // 构建 gRPC 地址
        let grpc_addr = format!(
            "{}:{}",
            container.container_ip,
            shared_types::GRPC_DEFAULT_PORT
        );

        // 提取 user_id（lookup_key 可能是 user_id 或 project_id）
        // 对于 ComputerAgentRunner，lookup_key 通常是 user_id
        let user_id = container_info
            .user_id()
            .map(|s| s.to_string())
            .unwrap_or_else(|| lookup_key.clone());

        let project_id = container_info.project_id().to_string();

        // 查询容器状态
        match query_container_status(&grpc_addr, &user_id, &project_id, &state.grpc_pool, config)
            .await
        {
            Ok(is_active) => {
                if is_active {
                    // 容器有活跃任务，更新活动时间
                    if let Err(e) = update_container_activity(&lookup_key, state).await {
                        warn!(
                            "⚠️ [STATUS_CHECKER] 更新活动时间失败: {}, {}",
                            lookup_key, e
                        );
                    } else {
                        updated += 1;
                        debug!(
                            "✅ [STATUS_CHECKER] 容器活跃，已更新活动时间: {}",
                            lookup_key
                        );
                    }
                } else {
                    debug!("📭 [STATUS_CHECKER] 容器空闲: {}", lookup_key);
                }
            }
            Err(e) => {
                failed += 1;
                debug!("❌ [STATUS_CHECKER] 查询失败: {}, {}", lookup_key, e);
            }
        }
    }

    info!(
        "📊 [STATUS_CHECKER] 检查完成: 总数={}, 已检查={}, 已更新={}, 失败={}",
        total_count, checked, updated, failed
    );

    Ok(())
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
) -> anyhow::Result<bool> {
    // 获取 gRPC 客户端
    let mut client = grpc_pool.get_client(grpc_addr).await?;

    // 构建请求
    let request = tonic::Request::new(GetContainerStatusRequest {
        user_id: user_id.to_string(),
        project_id: project_id.to_string(),
    });

    // 发送请求（带超时）
    let response = tokio::time::timeout(
        config.query_timeout,
        client.get_container_status(request),
    )
    .await??;

    let status_response = response.into_inner();

    debug!(
        "📊 [STATUS_CHECKER] 容器状态: user_id={}, is_active={}, active_tasks={}, status={}",
        user_id, status_response.is_active, status_response.active_tasks, status_response.status
    );

    // 如果容器有活跃任务，则认为容器活跃
    Ok(status_response.is_active || status_response.active_tasks > 0)
}

/// 更新容器活动时间
///
/// 使用写时复制模式更新 last_activity
async fn update_container_activity(
    lookup_key: &str,
    state: &Arc<AppState>,
) -> anyhow::Result<()> {
    use dashmap::mapref::entry::Entry;

    match state.project_and_agent_map.entry(lookup_key.to_string()) {
        Entry::Occupied(mut occupied) => {
            // 写时复制模式更新
            let mut new_info = (**occupied.get()).clone();
            new_info.update_activity();
            occupied.insert(Arc::new(new_info));
            Ok(())
        }
        Entry::Vacant(_) => {
            anyhow::bail!("容器记录不存在: {}", lookup_key);
        }
    }
}
