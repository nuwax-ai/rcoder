//! Pod 容器管理 HTTP 处理器
//!
//! 提供 Pod 容器的统计、启动和保活功能。
//!
//! ## 接口列表
//! - `GET /computer/pod/count` - 获取容器数量统计
//! - `POST /computer/pod/ensure` - 启动/确保容器存在（幂等）
//! - `POST /computer/pod/keepalive` - 容器保活（刷新活动时间）

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, info, instrument};
use utoipa::ToSchema;

use crate::router::AppState;
use crate::service::ComputerContainerManager;
use crate::{AppError, HttpResult};
use shared_types::{ProjectAndContainerInfo, ServiceResourceLimits};

// ============================================================================
// 常量定义
// ============================================================================

/// 默认清理超时时间（30分钟 = 1800秒）
const CLEANUP_TIMEOUT_SECONDS: u64 = 1800;

// ============================================================================
// 接口一：获取容器数量
// ============================================================================

/// 容器数量响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodCountResponse {
    /// 当前运行的容器总数
    #[schema(example = 5)]
    pub total_count: u32,

    /// 按服务类型分类的容器数量
    pub by_service_type: PodCountByServiceType,

    /// 统计时间戳 (Unix 毫秒)
    #[schema(example = 1702700000000_u64)]
    pub timestamp: u64,
}

/// 按服务类型分类的容器数量
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodCountByServiceType {
    /// RCoder 类型容器数量
    #[schema(example = 2)]
    pub rcoder: u32,

    /// ComputerAgentRunner 类型容器数量
    #[schema(example = 3)]
    pub computer_agent_runner: u32,
}

// ============================================================================
// 接口二：启动容器
// ============================================================================

/// 启动容器请求
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct EnsurePodRequest {
    /// 用户唯一标识符 (必填)
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目唯一标识符 (必填)
    #[schema(example = "proj_456")]
    pub project_id: String,

    /// 可选的资源限制配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<PodResourceLimits>,
}

/// Pod 资源限制配置
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PodResourceLimits {
    /// 内存限制 (bytes), 例如 4GB = 4294967296
    #[schema(example = 4294967296_u64)]
    pub memory: Option<u64>,

    /// CPU 份额 (1024 = 1 核)
    #[schema(example = 2048)]
    pub cpu_shares: Option<u64>,
}

/// 启动容器响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EnsurePodResponse {
    /// 容器是否为新创建 (false 表示已存在)
    pub created: bool,

    /// 容器基本信息
    pub container_info: PodContainerInfo,

    /// 提示消息
    #[schema(example = "容器已就绪，可通过 VNC 访问")]
    pub message: String,
}

/// 容器基本信息
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PodContainerInfo {
    /// 容器 ID
    #[schema(example = "abc123def456")]
    pub container_id: String,

    /// 容器名称
    #[schema(example = "computer-agent-runner-user_123")]
    pub container_name: String,

    /// 容器 IP 地址 (内部网络)
    #[schema(example = "172.17.0.5")]
    pub container_ip: String,

    /// 服务 URL
    #[schema(example = "http://172.17.0.5:8086")]
    pub service_url: String,

    /// 容器状态
    #[schema(example = "running")]
    pub status: String,
}

// ============================================================================
// 接口三：容器保活
// ============================================================================

/// 容器保活请求
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct KeepalivePodRequest {
    /// 用户唯一标识符
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目唯一标识符
    #[schema(example = "proj_456")]
    pub project_id: String,
}

/// 容器保活响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeepalivePodResponse {
    /// 容器是否已存在
    pub existed: bool,

    /// 容器是否为新创建 (当 existed=false 时为 true)
    pub created: bool,

    /// 容器基本信息
    pub container_info: PodContainerInfo,

    /// 上次活动时间 (Unix 毫秒时间戳, 更新前)
    #[schema(example = 1702700000000_u64)]
    pub previous_activity_time: u64,

    /// 当前活动时间 (Unix 毫秒时间戳, 更新后)
    #[schema(example = 1702700600000_u64)]
    pub current_activity_time: u64,

    /// 距离下次清理的剩余时间 (秒)
    #[schema(example = 1800)]
    pub time_until_cleanup: u64,

    /// 提示消息
    #[schema(example = "容器活动时间已刷新")]
    pub message: String,
}

// ============================================================================
// 接口四：重启容器
// ============================================================================

/// 重启容器请求
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RestartPodRequest {
    /// 用户唯一标识符 (必填)
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目唯一标识符 (必填)
    #[schema(example = "proj_456")]
    pub project_id: String,

    /// 可选的资源限制配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<PodResourceLimits>,
}

/// 重启容器响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RestartPodResponse {
    /// 容器是否为新创建 (之前不存在时为 true)
    pub was_existing: bool,

    /// 容器是否已重启
    pub restarted: bool,

    /// 容器基本信息
    pub container_info: PodContainerInfo,

    /// 提示消息
    #[schema(example = "容器已重启，可通过 VNC 访问虚拟桌面")]
    pub message: String,
}

// ============================================================================
// Handler 函数
// ============================================================================

/// 获取当前容器数量
///
/// 获取当前运行的容器总数及按服务类型分类的统计。
#[utoipa::path(
    get,
    path = "/computer/pod/count",
    responses(
        (status = 200, description = "成功获取容器数量", body = HttpResult<PodCountResponse>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_count",
    summary = "获取当前容器数量",
    description = "获取当前运行的容器总数及按服务类型分类的统计"
)]
#[axum::debug_handler]
pub async fn pod_count(
    State(_state): State<Arc<AppState>>,
) -> Result<HttpResult<PodCountResponse>, AppError> {
    debug!("📊 [POD_COUNT] 获取容器数量统计");

    // 获取全局 DockerManager
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("❌ [POD_COUNT] 获取 DockerManager 失败: {}", e);
            AppError::internal_server_error(&format!("获取 DockerManager 失败: {}", e))
        })?;

    // 获取所有容器列表
    let containers = docker_manager.list_containers();

    // 按服务类型统计
    let mut rcoder_count = 0u32;
    let mut computer_count = 0u32;

    for container in &containers {
        if container.container_name.starts_with("rcoder-agent-") {
            rcoder_count += 1;
        } else if container
            .container_name
            .starts_with("computer-agent-runner-")
        {
            computer_count += 1;
        }
    }

    let total_count = rcoder_count + computer_count;
    let timestamp = chrono::Utc::now().timestamp_millis() as u64;

    let response = PodCountResponse {
        total_count,
        by_service_type: PodCountByServiceType {
            rcoder: rcoder_count,
            computer_agent_runner: computer_count,
        },
        timestamp,
    };

    info!(
        "✅ [POD_COUNT] 容器统计完成: total={}, rcoder={}, computer_agent_runner={}",
        total_count, rcoder_count, computer_count
    );

    Ok(HttpResult::success(response))
}

/// 启动/确保容器存在（幂等）
///
/// 根据 user_id 和 project_id 启动或获取已存在的容器。
/// 仅启动容器，不启动 Agent 服务。
#[utoipa::path(
    post,
    path = "/computer/pod/ensure",
    request_body(content = EnsurePodRequest, description = "启动容器请求"),
    responses(
        (status = 200, description = "成功启动/获取容器", body = HttpResult<EnsurePodResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_ensure",
    summary = "启动/确保容器存在（幂等）",
    description = "根据 user_id 和 project_id 启动或获取已存在的容器，仅启动容器不启动 Agent 服务"
)]
#[axum::debug_handler]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_ensure(
    State(state): State<Arc<AppState>>,
    Json(request): Json<EnsurePodRequest>,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("❌ [POD_ENSURE] user_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id 不能为空",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("❌ [POD_ENSURE] project_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id 不能为空",
        ));
    }

    info!(
        "🚀 [POD_ENSURE] 确保容器存在: user_id={}, project_id={}",
        request.user_id, request.project_id
    );

    // 2. 检查容器是否存在
    let existing_container = ComputerContainerManager::get_container_info(&request.user_id).await?;

    let (container_info, created) = match existing_container {
        Some(info) => {
            info!(
                "📦 [POD_ENSURE] 容器已存在: container_id={}",
                info.container_id
            );
            (info, false)
        }
        None => {
            info!("🏗️ [POD_ENSURE] 创建新容器: user_id={}", request.user_id);

            // 转换资源限制
            let resource_limits = request.resource_limits.map(|limits| ServiceResourceLimits {
                memory_limit: limits.memory,
                cpu_limit: limits.cpu_shares.map(|c| c as f64 / 1024.0),
                swap_limit: None,
            });

            let info = ComputerContainerManager::get_or_create_container_for_user(
                &request.user_id,
                resource_limits,
            )
            .await?;

            info!(
                "✅ [POD_ENSURE] 容器创建成功: container_id={}",
                info.container_id
            );
            (info, true)
        }
    };

    // 3. 在 AppState 中记录容器信息（用于后续保活）
    if !state.project_and_agent_map.contains_key(&request.user_id) {
        // ComputerAgentRunner 模式：使用 project_id 创建，同时设置 user_id
        let mut project_info = ProjectAndContainerInfo::new(request.project_id.clone());
        project_info.set_user_id(Some(request.user_id.clone()));
        project_info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        project_info.set_container(Some(container_info.clone()));
        state
            .project_and_agent_map
            .insert(request.user_id.clone(), Arc::new(project_info));
    }

    // 4. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        container_name: container_info.container_name.clone(),
        container_ip: container_info.container_ip.clone(),
        service_url: container_info.service_url.clone(),
        status: container_info.status.clone(),
    };

    let message = if created {
        "容器创建成功，可通过 VNC 访问虚拟桌面（Agent 服务未启动）".to_string()
    } else {
        "容器已存在，可直接通过 VNC 访问虚拟桌面".to_string()
    };

    let response = EnsurePodResponse {
        created,
        container_info: pod_container_info,
        message,
    };

    Ok(HttpResult::success(response))
}

/// 容器保活（刷新活动时间）
///
/// 刷新容器的最后活动时间，防止被定时清理任务销毁。
/// 如果容器不存在会自动创建。
#[utoipa::path(
    post,
    path = "/computer/pod/keepalive",
    request_body(content = KeepalivePodRequest, description = "容器保活请求"),
    responses(
        (status = 200, description = "成功刷新活动时间", body = HttpResult<KeepalivePodResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_keepalive",
    summary = "容器保活（刷新活动时间）",
    description = "刷新容器的最后活动时间，防止被定时清理任务销毁。如果容器不存在会自动创建。"
)]
#[axum::debug_handler]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_keepalive(
    State(state): State<Arc<AppState>>,
    Json(request): Json<KeepalivePodRequest>,
) -> Result<HttpResult<KeepalivePodResponse>, AppError> {
    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("❌ [POD_KEEPALIVE] user_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id 不能为空",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("❌ [POD_KEEPALIVE] project_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id 不能为空",
        ));
    }

    info!(
        "💓 [POD_KEEPALIVE] 容器保活: user_id={}, project_id={}",
        request.user_id, request.project_id
    );

    let current_time = chrono::Utc::now();
    let current_time_millis = current_time.timestamp_millis() as u64;

    // 2. 检查 AppState 中是否有记录，并更新活动时间
    // 使用 Entry API 进行原子性操作，与 cleanup_task.rs 保持一致
    let (previous_activity_time, existed) = {
        use dashmap::mapref::entry::Entry;
        match state.project_and_agent_map.entry(request.user_id.clone()) {
            Entry::Occupied(mut occupied) => {
                let prev = occupied.get().last_activity().timestamp_millis() as u64;
                // 克隆并更新，然后替换整个 Arc（避免 Arc::make_mut 的潜在问题）
                let mut new_info = (**occupied.get()).clone();
                new_info.update_activity();
                occupied.insert(Arc::new(new_info));
                (prev, true)
            }
            Entry::Vacant(_) => (0u64, false),
        }
    };

    // 3. 获取或创建容器
    // 使用双重检查模式避免 TOCTOU 竞态条件
    let (container_info, created) = if !existed {
        // AppState 中没有记录，检查 Docker 中是否有容器
        let existing_container =
            ComputerContainerManager::get_container_info(&request.user_id).await?;

        match existing_container {
            Some(info) => {
                // Docker 中有容器，使用 Entry API 原子性地插入到 AppState
                // 双重检查：其他线程可能已经在 async 操作期间插入了记录
                use dashmap::mapref::entry::Entry;
                match state.project_and_agent_map.entry(request.user_id.clone()) {
                    Entry::Occupied(_) => {
                        // 其他线程已经插入，直接使用已有记录
                        info!("📦 [POD_KEEPALIVE] 容器已存在（Docker），AppState 已被其他线程更新");
                    }
                    Entry::Vacant(vacant) => {
                        // 原子性插入新记录
                        let mut project_info =
                            ProjectAndContainerInfo::new(request.project_id.clone());
                        project_info.set_user_id(Some(request.user_id.clone()));
                        project_info
                            .set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
                        project_info.set_container(Some(info.clone()));
                        vacant.insert(Arc::new(project_info));
                        info!("📦 [POD_KEEPALIVE] 容器已存在（Docker），已原子性添加到 AppState");
                    }
                }
                (info, false)
            }
            None => {
                // Docker 中也没有容器，创建新容器
                info!(
                    "🏗️ [POD_KEEPALIVE] 容器不存在，自动创建: user_id={}",
                    request.user_id
                );
                let info = ComputerContainerManager::get_or_create_container_for_user(
                    &request.user_id,
                    None,
                )
                .await?;

                // 使用 Entry API 原子性地插入到 AppState
                // 双重检查：其他线程可能已经在 async 操作期间插入了记录
                use dashmap::mapref::entry::Entry;
                match state.project_and_agent_map.entry(request.user_id.clone()) {
                    Entry::Occupied(_) => {
                        // 其他线程已经插入，直接使用已有记录
                        info!(
                            "✅ [POD_KEEPALIVE] 容器创建成功，AppState 已被其他线程更新: container_id={}",
                            info.container_id
                        );
                    }
                    Entry::Vacant(vacant) => {
                        // 原子性插入新记录
                        let mut project_info =
                            ProjectAndContainerInfo::new(request.project_id.clone());
                        project_info.set_user_id(Some(request.user_id.clone()));
                        project_info
                            .set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
                        project_info.set_container(Some(info.clone()));
                        vacant.insert(Arc::new(project_info));
                        info!(
                            "✅ [POD_KEEPALIVE] 容器创建成功，已原子性添加到 AppState: container_id={}",
                            info.container_id
                        );
                    }
                }
                (info, true)
            }
        }
    } else {
        // AppState 中有记录，直接获取容器信息
        let info = ComputerContainerManager::get_container_info(&request.user_id)
            .await?
            .ok_or_else(|| {
                error!("❌ [POD_KEEPALIVE] 容器状态异常：AppState 有记录但 Docker 中找不到容器");
                AppError::internal_server_error("容器状态异常")
            })?;
        (info, false)
    };

    // 4. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        container_name: container_info.container_name.clone(),
        container_ip: container_info.container_ip.clone(),
        service_url: container_info.service_url.clone(),
        status: container_info.status.clone(),
    };

    let message = if created {
        format!(
            "容器已自动创建，距离自动清理还有 {} 分钟",
            CLEANUP_TIMEOUT_SECONDS / 60
        )
    } else {
        format!(
            "容器活动时间已刷新，距离自动清理还有 {} 分钟",
            CLEANUP_TIMEOUT_SECONDS / 60
        )
    };

    let response = KeepalivePodResponse {
        existed: !created,
        created,
        container_info: pod_container_info,
        previous_activity_time,
        current_activity_time: current_time_millis,
        time_until_cleanup: CLEANUP_TIMEOUT_SECONDS,
        message,
    };

    info!(
        "✅ [POD_KEEPALIVE] 保活完成: existed={}, created={}, time_until_cleanup={}s",
        !created, created, CLEANUP_TIMEOUT_SECONDS
    );

    Ok(HttpResult::success(response))
}

/// 重启容器（销毁后重建）
///
/// 根据 user_id 和 project_id 重启容器。
/// 如果容器存在，先销毁再创建新容器；如果不存在，直接创建。
#[utoipa::path(
    post,
    path = "/computer/pod/restart",
    request_body(content = RestartPodRequest, description = "重启容器请求"),
    responses(
        (status = 200, description = "成功重启容器", body = HttpResult<RestartPodResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_restart",
    summary = "重启容器（销毁后重建）",
    description = "根据 user_id 和 project_id 重启容器。如果容器存在，先销毁再创建新容器；如果不存在，直接创建。"
)]
#[axum::debug_handler]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_restart(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RestartPodRequest>,
) -> Result<HttpResult<RestartPodResponse>, AppError> {
    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("❌ [POD_RESTART] user_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id 不能为空",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("❌ [POD_RESTART] project_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id 不能为空",
        ));
    }

    info!(
        "🔄 [POD_RESTART] 重启容器: user_id={}, project_id={}",
        request.user_id, request.project_id
    );

    // 2. 检查容器是否存在
    let existing_container = ComputerContainerManager::get_container_info(&request.user_id).await?;
    let was_existing = existing_container.is_some();

    // 3. 如果容器存在，先销毁
    if let Some(container_info) = existing_container {
        info!(
            "🗑️ [POD_RESTART] 销毁现有容器: container_id={}",
            container_info.container_id
        );

        // 从 AppState 中移除记录
        state.project_and_agent_map.remove(&request.user_id);

        // 获取 DockerManager 并停止容器
        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                error!("❌ [POD_RESTART] 获取 DockerManager 失败: {}", e);
                AppError::internal_server_error(&format!("获取 DockerManager 失败: {}", e))
            })?;

        // 使用 container_stop 模块的运行时清理策略
        if let Err(e) = docker_manager::container_stop::runtime_cleanup_container(
            &docker_manager,
            &container_info.container_id,
        )
        .await
        {
            // 记录错误但继续尝试创建新容器
            error!(
                "⚠️ [POD_RESTART] 停止容器失败（继续创建新容器）: container_id={}, error={}",
                container_info.container_id, e
            );
        } else {
            info!(
                "✅ [POD_RESTART] 容器已销毁: container_id={}",
                container_info.container_id
            );
        }

        // 等待一小段时间确保容器资源释放
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // 4. 创建新容器
    info!("🏗️ [POD_RESTART] 创建新容器: user_id={}", request.user_id);

    // 转换资源限制
    let resource_limits = request.resource_limits.map(|limits| ServiceResourceLimits {
        memory_limit: limits.memory,
        cpu_limit: limits.cpu_shares.map(|c| c as f64 / 1024.0),
        swap_limit: None,
    });

    let container_info = ComputerContainerManager::get_or_create_container_for_user(
        &request.user_id,
        resource_limits,
    )
    .await?;

    info!(
        "✅ [POD_RESTART] 新容器创建成功: container_id={}",
        container_info.container_id
    );

    // 5. 在 AppState 中记录容器信息（使用 Entry API 原子性操作）
    {
        use dashmap::mapref::entry::Entry;
        let mut project_info = ProjectAndContainerInfo::new(request.project_id.clone());
        project_info.set_user_id(Some(request.user_id.clone()));
        project_info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        project_info.set_container(Some(container_info.clone()));

        match state.project_and_agent_map.entry(request.user_id.clone()) {
            Entry::Occupied(mut occupied) => {
                // 已存在记录，更新为新容器信息
                occupied.insert(Arc::new(project_info));
            }
            Entry::Vacant(vacant) => {
                // 不存在记录，插入新记录
                vacant.insert(Arc::new(project_info));
            }
        }
    }

    // 6. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        container_name: container_info.container_name.clone(),
        container_ip: container_info.container_ip.clone(),
        service_url: container_info.service_url.clone(),
        status: container_info.status.clone(),
    };

    let message = if was_existing {
        "容器已重启，可通过 VNC 访问虚拟桌面（Agent 服务未启动）".to_string()
    } else {
        "容器创建成功（之前不存在），可通过 VNC 访问虚拟桌面（Agent 服务未启动）".to_string()
    };

    let response = RestartPodResponse {
        was_existing,
        restarted: true,
        container_info: pod_container_info,
        message,
    };

    info!(
        "✅ [POD_RESTART] 完成: was_existing={}, container_id={}",
        was_existing, container_info.container_id
    );

    Ok(HttpResult::success(response))
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pod_count_by_service_type_default() {
        let count = PodCountByServiceType {
            rcoder: 0,
            computer_agent_runner: 0,
        };
        assert_eq!(count.rcoder + count.computer_agent_runner, 0);
    }

    #[test]
    fn test_pod_resource_limits_serialization() {
        let limits = PodResourceLimits {
            memory: Some(4294967296),
            cpu_shares: Some(2048),
        };

        let json = serde_json::to_string(&limits).unwrap();
        assert!(json.contains("4294967296"));
        assert!(json.contains("2048"));
    }

    #[test]
    fn test_ensure_pod_response_serialization() {
        let response = EnsurePodResponse {
            created: true,
            container_info: PodContainerInfo {
                container_id: "abc123".to_string(),
                container_name: "computer-agent-runner-user_123".to_string(),
                container_ip: "172.17.0.5".to_string(),
                service_url: "http://172.17.0.5:8086".to_string(),
                status: "running".to_string(),
            },
            message: "容器创建成功".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("created"));
        assert!(json.contains("container_info"));
        assert!(json.contains("message"));
    }
}
