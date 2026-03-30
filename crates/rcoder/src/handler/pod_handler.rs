//! Pod 容器管理 HTTP 处理器
//!
//! 提供 Pod 容器的统计、启动和保活功能。
//!
//! ## 接口列表
//! - `GET /computer/pod/count` - 获取容器数量统计
//! - `GET /computer/pod/list` - 获取所有容器信息（支持分页）
//! - `POST /computer/pod/ensure` - 启动/确保容器存在（幂等）
//! - `POST /computer/pod/keepalive` - 容器保活（刷新活动时间）

use axum::extract::State;
use axum::{Json, extract::Query};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, instrument, warn};
use utoipa::{IntoParams, ToSchema};

use super::utils::extract_grpc_addr_with_port;
use crate::router::AppState;
use crate::service::ComputerContainerManager;
use crate::service::vnc_sync::sync_single_vnc_backend;
use crate::{AppError, HttpResult};
use shared_types::{ProjectAndContainerInfo, ServiceResourceLimits};

// ============================================================================
// 辅助函数
// ============================================================================

/// 验证 Pod 资源限制配置
///
/// # 参数
/// * `limits` - 资源限制配置
///
/// # 返回
/// Ok(()) 验证通过，Err(String) 返回错误信息
fn validate_resource_limits(limits: &PodResourceLimits) -> Result<(), String> {
    // 验证 CPU 限制
    if let Some(cpu) = limits.cpu {
        if cpu <= 0.0 {
            return Err("cpu must be greater than 0".to_string());
        }
        if cpu > 128.0 {
            return Err("cpu cannot exceed 128 cores".to_string());
        }
    }

    // 验证内存限制
    if let Some(memory) = limits.memory {
        if memory < 512_000_000.0 {
            return Err("memory must be at least 512MB".to_string());
        }
        if memory > 128_000_000_000.0 {
            return Err("memory cannot exceed 128GB".to_string());
        }
    }

    // 验证 swap 限制
    if let Some(swap) = limits.swap {
        if swap < 512_000_000.0 {
            return Err("swap must be at least 512MB".to_string());
        }
        // swap 必须 >= memory（如果两者都设置了）
        if let (Some(memory), Some(swap_val)) = (limits.memory, limits.swap) {
            if swap_val < memory {
                return Err("swap should be >= memory".to_string());
            }
        }
    }

    Ok(())
}

/// 将 Unix 毫秒时间戳转换为东八区（UTC+8）时间字符串
///
/// # 参数
/// * `timestamp_millis` - Unix 毫秒时间戳
///
/// # 返回
/// 格式为 "YYYY-MM-DD HH:MM:SS" 的时间字符串
fn timestamp_to_utc8_string(timestamp_millis: u64) -> String {
    use chrono::{DateTime, FixedOffset};

    // 直接从毫秒时间戳创建 DateTime<Utc>
    let datetime = DateTime::from_timestamp_millis(timestamp_millis as i64)
        .unwrap_or_else(|| DateTime::UNIX_EPOCH);

    // 创建东八区时区偏移 (UTC+8)
    // 注意: east_opt 在参数有效时总是返回 Some，这里使用 unwrap_or 仅作为安全保障
    let utc8_offset = FixedOffset::east_opt(8 * 3600).unwrap_or_else(|| {
 tracing::warn!("created UTC+8 message failed, message UTC+0");
        FixedOffset::east_opt(0).unwrap_or(FixedOffset::east_opt(0).unwrap())
    });

    // 转换为东八区时间并格式化
    datetime
        .with_timezone(&utc8_offset)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

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
// 接口二：获取所有容器信息
// ============================================================================

/// 获取容器列表的查询参数
#[derive(Debug, Clone, Deserialize, IntoParams, ToSchema)]
pub struct PodListQuery {
    /// 分页大小（默认100，不传则返回所有）
    #[param(example = 100)]
    #[schema(example = 100)]
    #[serde(default)]
    pub limit: Option<u32>,
}

/// 容器详细信息
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PodDetailInfo {
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

    /// 服务类型
    #[schema(example = "ComputerAgentRunner")]
    pub service_type: String,

    /// 项目 ID（如果有）
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 用户 ID（如果有）
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 创建时间 (Unix 毫秒时间戳)
    #[schema(example = 1702700000000_u64)]
    pub created_at: u64,

    /// 最后活动时间 (Unix 毫秒时间戳)
    #[schema(example = 1702700600000_u64)]
    pub last_activity: Option<u64>,

    /// 镜像名称
    #[schema(example = "rcoder-agent-runner:latest")]
    pub image: Option<String>,

    /// 内部端口
    #[schema(example = 8086)]
    pub internal_port: Option<u16>,

    /// 外部端口
    #[schema(example = 30001)]
    pub external_port: Option<u16>,
}

/// 获取容器列表响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PodListResponse {
    /// 容器列表
    pub containers: Vec<PodDetailInfo>,

    /// 总数量
    #[schema(example = 5)]
    pub total: u32,

    /// 返回数量
    #[schema(example = 5)]
    pub returned: u32,

    /// 是否已分页
    #[schema(example = false)]
    pub paginated: bool,

    /// 查询时间戳 (Unix 毫秒)
    #[schema(example = 1702700000000_u64)]
    pub timestamp: u64,
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
    /// 内存限制 (bytes), 例如 4GB = 4294967296，支持浮点数输入
    #[schema(example = 4294967296.0)]
    pub memory: Option<f64>,

    /// CPU 限制（核心数）, 例如 1.5 表示 1.5 核
    #[schema(example = 2.0)]
    pub cpu: Option<f64>,

    /// 交换空间限制 (bytes), 例如 2GB = 2147483648，支持浮点数输入
    #[schema(example = 2147483648.0)]
    pub swap: Option<f64>,
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

/// 容器基本信息（对外接口）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PodContainerInfo {
    /// 容器 ID
    #[schema(example = "abc123def456")]
    pub container_id: String,

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

    /// 上次活动时间 (东八区时间字符串)
    #[schema(example = "2023-12-16 10:00:00")]
    pub previous_activity_time_str: String,

    /// 当前活动时间 (东八区时间字符串)
    #[schema(example = "2023-12-16 10:10:00")]
    pub current_activity_time_str: String,

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
        (status = 401, description = "API Key 鉴权失败", body = String),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_count",
    summary = "获取当前容器数量",
    description = "获取当前运行的容器总数及按服务类型分类的统计"
)]
pub async fn pod_count(
    State(_state): State<Arc<AppState>>,
) -> Result<HttpResult<PodCountResponse>, AppError> {
 debug!("📊 [POD_COUNT] getcontainer message ");

    // 获取全局 DockerManager
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("[POD_COUNT] Failed to get DockerManager: {}", e);
            AppError::internal_server_error(&format!("Failed to get DockerManager: {}", e))
        })?;

    // 获取所有容器列表
    let containers = docker_manager.list_containers().await;

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

    debug!(
        "✅ [POD_COUNT] 容器统计完成: total={}, rcoder={}, computer_agent_runner={}",
        total_count, rcoder_count, computer_count
    );

    Ok(HttpResult::success(response))
}

/// 获取所有容器信息
///
/// 获取所有容器的详细信息，支持可选的分页查询（默认100条）。
/// 如果不传 limit 参数，则返回所有容器。
#[utoipa::path(
    get,
    path = "/computer/pod/list",
    params(
        PodListQuery
    ),
    responses(
        (status = 200, description = "成功获取容器列表", body = HttpResult<PodListResponse>),
        (status = 401, description = "API Key 鉴权失败", body = String),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_list",
    summary = "获取所有容器信息",
    description = "获取所有容器的详细信息，支持可选的分页查询（默认100条）。如果不传 limit 参数，则返回所有容器。"
)]
#[instrument(skip(state))]
pub async fn pod_list(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PodListQuery>,
) -> Result<HttpResult<PodListResponse>, AppError> {
 debug!("📋 [POD_LIST] getcontainer message : limit={:?}", params.limit);

    // 1. 获取 Docker 容器列表
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("[POD_LIST] Failed to get DockerManager: {}", e);
            AppError::internal_server_error(&format!("Failed to get DockerManager: {}", e))
        })?;

    let docker_containers = docker_manager.list_containers().await;

    // 2. 获取 DuckDB 存储中的容器记录
    let duckdb_containers = state.projects.get_all_container_records().map_err(|e| {
        error!("[POD_LIST] Failed to get DuckDB container list: {}", e);
        AppError::internal_server_error(&format!("Failed to get DuckDB container list: {}", e))
    })?;

    // 3. 创建容器ID到DuckDB记录的映射
    let mut duckdb_map: std::collections::HashMap<String, &duckdb_manager::ContainerRecord> =
        std::collections::HashMap::new();
    for record in &duckdb_containers {
        duckdb_map.insert(record.container_id.clone(), record);
    }

    // 4. 合并数据，构建容器详细信息列表
    let mut containers: Vec<PodDetailInfo> = Vec::new();

    for docker_container in &docker_containers {
        let duckdb_record = duckdb_map.get(&docker_container.container_id);

        // 确定服务类型
        let service_type = if docker_container.container_name.starts_with("rcoder-agent-") {
            "RCoder"
        } else if docker_container
            .container_name
            .starts_with("computer-agent-runner-")
        {
            "ComputerAgentRunner"
        } else {
            "Unknown"
        };

        // 从容器名称提取 user_id（如果是 computer-agent-runner-{user_id}）
        let user_id = if docker_container
            .container_name
            .starts_with("computer-agent-runner-")
        {
            docker_container
                .container_name
                .strip_prefix("computer-agent-runner-")
                .map(|s| s.to_string())
        } else {
            None
        };

        // 获取项目ID和用户ID（从DuckDB或Docker容器信息）
        let project_id = duckdb_record
            .and_then(|r| {
                // 尝试从DuckDB关联的项目中获取project_id
                state
                    .projects
                    .get_projects_by_container_id(&r.container_id)
                    .ok()
                    .and_then(|projects| projects.first().map(|p| p.project_id.clone()))
            })
            .or_else(|| {
                // 如果DuckDB中没有，使用Docker容器中的project_id
                if !docker_container.project_id.is_empty()
                    && docker_container.project_id != "unknown"
                {
                    Some(docker_container.project_id.clone())
                } else {
                    None
                }
            });

        let final_user_id = user_id.or_else(|| {
            duckdb_record.and_then(|r| {
                state
                    .projects
                    .get_projects_by_container_id(&r.container_id)
                    .ok()
                    .and_then(|projects| {
                        projects
                            .first()
                            .and_then(|p| p.user_id.as_ref().map(|s| s.clone()))
                    })
            })
        });

        // 构建容器详细信息
        let container_info = PodDetailInfo {
            container_id: docker_container.container_id.clone(),
            container_name: docker_container.container_name.clone(),
            container_ip: duckdb_record
                .map(|r| r.container_ip.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            service_url: duckdb_record
                .map(|r| r.service_url.clone())
                .unwrap_or_else(|| {
                    format!(
                        "http://{}:{}",
                        docker_container.assigned_port, docker_container.internal_port
                    )
                }),
            status: match &docker_container.status {
                docker_manager::ContainerStatus::Running => "running".to_string(),
                docker_manager::ContainerStatus::Stopped => "stopped".to_string(),
                docker_manager::ContainerStatus::Creating => "creating".to_string(),
                docker_manager::ContainerStatus::Paused => "paused".to_string(),
                docker_manager::ContainerStatus::Restarting => "restarting".to_string(),
                docker_manager::ContainerStatus::Removing => "removing".to_string(),
                docker_manager::ContainerStatus::Exited => "exited".to_string(),
                docker_manager::ContainerStatus::Dead => "dead".to_string(),
                docker_manager::ContainerStatus::Unknown(s) => format!("unknown:{}", s),
            },
            service_type: service_type.to_string(),
            project_id,
            user_id: final_user_id,
            created_at: docker_container.created_at.timestamp_millis() as u64,
            last_activity: duckdb_record.map(|r| r.last_activity.timestamp_millis() as u64),
            image: Some(docker_container.image.clone()),
            internal_port: Some(docker_container.internal_port),
            external_port: if docker_container.assigned_port > 0 {
                Some(docker_container.assigned_port)
            } else {
                duckdb_record.map(|r| r.external_port)
            },
        };

        containers.push(container_info);
    }

    // 5. 按创建时间倒序排序（最新的在前）
    containers.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // 6. 应用分页
    let total = containers.len() as u32;
    let limit = params.limit.unwrap_or(0);
    let paginated = limit > 0;
    let returned = if paginated {
        containers.truncate(limit as usize);
        limit.min(total)
    } else {
        total
    };

    let timestamp = chrono::Utc::now().timestamp_millis() as u64;

    let response = PodListResponse {
        containers,
        total,
        returned,
        paginated,
        timestamp,
    };

    info!(
        "✅ [POD_LIST] 容器列表获取完成: total={}, returned={}, paginated={}",
        total, returned, paginated
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
        (status = 401, description = "API Key 鉴权失败", body = String),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_ensure",
    summary = "启动/确保容器存在（幂等）",
    description = "根据 user_id 和 project_id 启动或获取已存在的容器，仅启动容器不启动 Agent 服务"
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_ensure(
    State(state): State<Arc<AppState>>,
    Json(request): Json<EnsurePodRequest>,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_ENSURE] user_id is required");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id is required",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_ENSURE] project_id is required");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id is required",
        ));
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits {
        if let Err(e) = validate_resource_limits(limits) {
 error!("[POD_ENSURE] resources message failed: {}", e);
            return Ok(HttpResult::error(
                shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
                &format!("Invalid resource_limits: {}", e),
            ));
        }
    }

    info!(
        "🚀 [POD_ENSURE] 确保容器存在: user_id={}, project_id={}",
        request.user_id, request.project_id
    );

    // === 并发保护：检查是否有其他请求正在创建同一用户的容器 ===
    // 使用原子标记（DashMap）避免并发请求互相干扰，无死锁风险
    if let Some(creating_since) = state.pod_creating.get(&request.user_id) {
        let elapsed = creating_since.elapsed();
        drop(creating_since); // 释放 DashMap ref

        // 标记超过 60 秒视为过期（创建方可能已崩溃），忽略并继续
        if elapsed < std::time::Duration::from_secs(60) {
            info!(
                "⏳ [POD_ENSURE] 容器正在创建中，等待完成: user_id={}, 已等待={:?}",
                request.user_id, elapsed
            );

            // 轮询等待容器就绪（最多等 30 秒，每秒检查一次）
            let mut waited_container_info = None;
            for wait_sec in 1..=30 {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                // 标记已被移除 = 创建完成
                if !state.pod_creating.contains_key(&request.user_id) {
                    // 尝试获取容器信息
                    let docker_mgr = docker_manager::global::get_global_docker_manager()
                        .await
                        .ok();
                    if let Some(mgr) = docker_mgr {
                        if let Ok(Some(info)) = mgr.get_user_container_info(&request.user_id).await
                        {
                            info!(
                                "✅ [POD_ENSURE] 等待成功，容器已就绪（等待{}秒）: user_id={}, container_id={}",
                                wait_sec, request.user_id, info.container_id
                            );
                            waited_container_info = Some(info);
                            break;
                        }
                    }
                }

                if wait_sec % 5 == 0 {
                    debug!(
                        "[POD_ENSURE] 仍在等待容器创建: user_id={}, 第{}秒",
                        request.user_id, wait_sec
                    );
                }
            }

            // 如果等待成功，直接使用已就绪的容器，跳过创建流程
            if let Some(info) = waited_container_info {
                // 同步 VNC 后端映射
                if let Some(ref pingora_service) = state.pingora_service {
                    sync_single_vnc_backend(pingora_service, &request.user_id, &info.container_ip)
                        .await;
                    info!(
                        "🔄 [POD_ENSURE] VNC 后端映射已同步: user_id={} -> {}",
                        request.user_id, info.container_ip
                    );
                }

                // 更新 DuckDB 记录
                let project_info = if let Some(existing) = state.get_project(&request.project_id) {
                    let mut pinfo = (*existing).clone();
                    pinfo.set_container(Some(info.clone()));
                    pinfo
                } else {
                    let mut pinfo = ProjectAndContainerInfo::new(request.project_id.clone());
                    pinfo.set_user_id(Some(request.user_id.clone()));
                    pinfo.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
                    pinfo.set_container(Some(info.clone()));
                    pinfo
                };
                state.insert_project(request.project_id.clone(), Arc::new(project_info));
                debug!(
                    "📝 [POD_ENSURE] DuckDB 记录已更新: project_id={}, user_id={}, container_id={}",
                    request.project_id, request.user_id, info.container_id
                );

                // 返回成功响应
                let pod_container_info = PodContainerInfo {
                    container_id: info.container_id.clone(),
                    status: info.status.clone(),
                };
                return Ok(HttpResult::success(EnsurePodResponse {
                    created: false,
                    container_info: pod_container_info,
                    message: format!(
                        "容器已就绪（等待其他请求创建完成）: container_id={}",
                        info.container_id
                    ),
                }));
            }
            // 等待超时，继续正常的创建流程（此时标记可能已过期被清理）
            warn!(
                "⚠️ [POD_ENSURE] 等待容器创建超时（30秒），将继续尝试创建: user_id={}",
                request.user_id
            );
        } else {
            // 标记过期，清理后继续
            warn!(
                "⚠️ [POD_ENSURE] 创建标记已过期（{:?}），清理并继续",
                elapsed
            );
            state.pod_creating.remove(&request.user_id);
        }
    }

    // 2. 🔍 实时查询 Docker API 检查容器是否存在（不依赖缓存）
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("[POD_ENSURE] Failed to get DockerManager: {}", e);
            AppError::internal_server_error(&format!("Failed to get DockerManager: {}", e))
        })?;

    // 通过 find_user_container 查询容器（使用 ServiceConfig 中配置的容器前缀）
    let existing_container = docker_manager
        .find_user_container(
            &request.user_id,
            &shared_types::ServiceType::ComputerAgentRunner,
        )
        .await
        .map_err(|e| {
            error!("[POD_ENSURE] Failed to query container status: {}", e);
            AppError::internal_server_error(&format!("Failed to query container status: {}", e))
        })?;

    // 判断是否需要创建新容器
    let need_create = match existing_container {
        Some(result) if result.is_running => {
            // 容器存在且正在运行，无需创建
            info!(
                "📦 [POD_ENSURE] 容器已存在且运行中: container_id={}, status={:?}",
                result.container_id, result.status
            );
            false
        }
        Some(result) => {
            // 容器存在但未运行（Exited 等状态），需要删除并重建
            warn!(
                "⚠️ [POD_ENSURE] 容器存在但未运行: container_id={}, status={:?}, 将删除并重建",
                result.container_id, result.status
            );

            // 删除旧容器
            // 如果删除失败（包括容器不存在等情况），返回错误让调用者知道
            docker_manager
                .stop_container_by_id(&result.container_id)
                .await
                .map_err(|e| {
                    error!(
                        "❌ [POD_ENSURE] Failed to delete old container: container_id={}, error={}",
                        result.container_id, e
                    );
                    AppError::internal_server_error(&format!(
                        "Failed to delete old container: {}",
                        e
                    ))
                })?;

            info!(
                "✅ [POD_ENSURE] 旧容器已删除: container_id={}",
                result.container_id
            );

            // ⏱️ 等待 Docker 完全释放容器资源（避免竞态条件）
            // Docker 删除是异步操作，立即创建同名容器可能导致资源冲突
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
 debug!("⏱️ [POD_ENSURE] already message containerresourcesreleased");

            true
        }
        None => {
            // 容器不存在，需要创建
 info!("🏗️ [POD_ENSURE] containernot found, message created message container");
            true
        }
    };

    // 3. 获取或创建容器（带重试机制 + 标记）
    let (container_info, created) = if need_create {
        // 🆕 设置创建标记，防止并发请求重复创建
        state
            .pod_creating
            .insert(request.user_id.clone(), Instant::now());

        // 创建新容器，最多重试 3 次
        let resource_limits = request.resource_limits.map(|limits| ServiceResourceLimits {
            memory_limit: limits.memory,
            cpu_limit: limits.cpu,
            swap_limit: limits.swap,
        });

        let mut last_error = None;
        let mut result = None;
        let max_attempts = 3;

        for attempt in 1..=max_attempts {
            match ComputerContainerManager::get_or_create_container_for_user(
                &request.user_id,
                resource_limits.clone(),
            )
            .await
            {
                Ok(info) => {
                    if attempt > 1 {
                        info!(
                            "✅ [POD_ENSURE] 容器创建成功（第 {} 次尝试）: container_id={}",
                            attempt, info.container_id
                        );
                    } else {
                        info!(
                            "✅ [POD_ENSURE] 容器创建成功: container_id={}",
                            info.container_id
                        );
                    }
                    result = Some(info);
                    break;
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < max_attempts {
                        warn!(
                            "⚠️ [POD_ENSURE] 容器创建失败（第 {} 次尝试），将重试: {}",
                            attempt,
                            last_error
                                .as_ref()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "未知错误".to_string())
                        );
                        // 等待一段时间后重试（指数退避）
                        tokio::time::sleep(tokio::time::Duration::from_millis(
                            200 * attempt as u64,
                        ))
                        .await;
                    } else {
 error!("[POD_ENSURE] containercreatedfailed(alreadyretry {} message )", max_attempts);
                    }
                }
            }
        }

        // 返回结果或错误
        match result {
            Some(info) => {
                // 创建成功，清除标记
                state.pod_creating.remove(&request.user_id);
                (info, true)
            }
            None => {
                // 创建失败，也要清除标记
                state.pod_creating.remove(&request.user_id);
                let error_msg = last_error
                    .as_ref()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "容器创建失败但未捕获到错误信息".to_string());
                return Err(AppError::internal_server_error(&error_msg));
            }
        }
    } else {
        // 获取现有容器的完整信息
        match docker_manager
            .get_user_container_info(&request.user_id)
            .await
        {
            Ok(Some(info)) => {
                // 容器信息正常获取
                (info, false)
            }
            Ok(None) => {
                // Docker API 确认容器在运行，但内部 map 还没同步
                // 短暂等待让内部 map 同步，而不是直接重建
                warn!(
                    "⚠️ [POD_ENSURE] 容器运行中但内部映射未就绪，等待同步: user_id={}",
                    request.user_id
                );

                let mut retry_info = None;
                for retry_attempt in 1..=3 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    match docker_manager
                        .get_user_container_info(&request.user_id)
                        .await
                    {
                        Ok(Some(info)) => {
                            info!(
                                "✅ [POD_ENSURE] 内部映射已同步（第{}次重试）: container_id={}",
                                retry_attempt, info.container_id
                            );
                            retry_info = Some(info);
                            break;
                        }
                        _ => {
 debug!("[POD_ENSURE] message mapping message not message : message {} message ", retry_attempt);
                        }
                    }
                }

                match retry_info {
                    Some(info) => (info, false),
                    None => {
                        // 3次重试后仍失败，才考虑重建
                        warn!(
                            "⚠️ [POD_ENSURE] 等待同步超时，尝试重新创建: user_id={}",
                            request.user_id
                        );

                        let resource_limits =
                            request.resource_limits.map(|limits| ServiceResourceLimits {
                                memory_limit: limits.memory,
                                cpu_limit: limits.cpu,
                                swap_limit: limits.swap,
                            });

                        // 设置创建标记
                        state
                            .pod_creating
                            .insert(request.user_id.clone(), std::time::Instant::now());

                        let result = ComputerContainerManager::get_or_create_container_for_user(
                            &request.user_id,
                            resource_limits,
                        )
                        .await;

                        // 清除创建标记
                        state.pod_creating.remove(&request.user_id);

                        match result {
                            Ok(info) => {
                                info!(
                                    "✅ [POD_ENSURE] 容器重新创建成功: container_id={}",
                                    info.container_id
                                );
                                (info, true)
                            }
                            Err(e) => {
                                error!(
                                    "❌ [POD_ENSURE] 容器重新创建失败: user_id={}, error={}",
                                    request.user_id, e
                                );
                                return Err(e);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!(
                    "❌ [POD_ENSURE] 获取容器完整信息失败: user_id={}, error={}",
                    request.user_id, e
                );
                return Err(AppError::internal_server_error(&format!(
                    "获取容器完整信息失败: {}",
                    e
                )));
            }
        }
    };

    // 4. 🆕 如果是新创建的容器，立即同步 VNC 后端映射
    // 这消除了等待定时同步任务（最多 5 秒）的空窗期
    if created {
        if let Some(ref pingora_service) = state.pingora_service {
            sync_single_vnc_backend(
                pingora_service,
                &request.user_id,
                &container_info.container_ip,
            )
            .await;
            info!(
                "🔄 [POD_ENSURE] VNC 后端映射已同步: user_id={} -> {}",
                request.user_id, container_info.container_ip
            );
        }
    }

    // 5. 更新 DuckDB 存储中的容器信息（用于后续保活）
    // 无论容器是新建还是已存在，都要确保 DuckDB 记录是最新的
    let project_info = if let Some(existing) = state.get_project(&request.project_id) {
        // 如果已存在记录，更新容器信息
        let mut info = (*existing).clone();
        info.set_container(Some(container_info.clone()));
        info
    } else {
        // 如果不存在记录，创建新记录
        let mut info = ProjectAndContainerInfo::new(request.project_id.clone());
        info.set_user_id(Some(request.user_id.clone()));
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_container(Some(container_info.clone()));
        info
    };

    state.insert_project(request.project_id.clone(), Arc::new(project_info));
    debug!(
        "📝 [POD_ENSURE] DuckDB 记录已更新: project_id={}, user_id={}, container_id={}",
        request.project_id, request.user_id, container_info.container_id
    );

    // 6. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
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
        (status = 401, description = "API Key 鉴权失败", body = String),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_keepalive",
    summary = "容器保活（刷新活动时间）",
    description = "刷新容器的最后活动时间，防止被定时清理任务销毁。如果容器不存在会返回错误。"
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_keepalive(
    State(state): State<Arc<AppState>>,
    Json(request): Json<KeepalivePodRequest>,
) -> Result<HttpResult<KeepalivePodResponse>, AppError> {
    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_KEEPALIVE] user_id is required");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id is required",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_KEEPALIVE] project_id is required");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id is required",
        ));
    }

    info!(
        "💓 [POD_KEEPALIVE] 容器保活: user_id={}, project_id={}",
        request.user_id, request.project_id
    );

    // 2. 检查 DuckDB 存储中是否有记录，并更新活动时间
    let (previous_activity_time, current_activity_time, existed) = {
        if let Some(existing_info) = state.get_project(&request.project_id) {
            let prev = existing_info.last_activity().timestamp_millis() as u64;

            // 获取实际更新的时间
            let updated_time = state.update_activity(&request.project_id);
            let current = updated_time
                .map(|t| t.timestamp_millis() as u64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as u64);

            (prev, current, true)
        } else {
            let now = chrono::Utc::now().timestamp_millis() as u64;
            (0u64, now, false)
        }
    };

    // 3. 获取或创建容器
    let (container_info, created) = if !existed {
        // DuckDB 存储中没有记录，检查 Docker 中是否有容器
        let existing_container =
            ComputerContainerManager::get_container_info(&request.user_id).await?;

        match existing_container {
            Some(info) => {
                // Docker 中有容器，检查并插入到 DuckDB 存储
                if !state.contains_project(&request.project_id) {
                    let mut project_info = ProjectAndContainerInfo::new(request.project_id.clone());
                    project_info.set_user_id(Some(request.user_id.clone()));
                    project_info
                        .set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
                    project_info.set_container(Some(info.clone()));
                    state.insert_project(request.project_id.clone(), Arc::new(project_info));
 info!("[POD_KEEPALIVE] containeralreadyexists(Docker), already message DuckDB message ");
                } else {
 info!("[POD_KEEPALIVE] containeralreadyexists(Docker), DuckDB already message ");
                }
                (info, false)
            }
            None => {
                // Docker 中也没有容器，返回错误而不是创建新容器
 info!("❌ [POD_KEEPALIVE] containernot found: user_id={}", request.user_id);
                return Ok(HttpResult::error(
                    shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                    &format!(
                        "找不到用户 {} 的容器，请先发送聊天请求创建容器",
                        request.user_id
                    ),
                ));
            }
        }
    } else {
        // DuckDB 存储中有记录，直接获取容器信息
        let info = ComputerContainerManager::get_container_info(&request.user_id)
            .await?
            .ok_or_else(|| {
                error!(
                    "[POD_KEEPALIVE] Container status abnormal：DuckDB 有记录但 Docker 中找不到容器"
                );
                AppError::internal_server_error("Container status abnormal")
            })?;
        (info, false)
    };

    // 4. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        status: container_info.status.clone(),
    };

    // 从配置中获取清理超时时间
    let idle_timeout_seconds = state.config.cleanup_config.idle_timeout_seconds;

    let message = if created {
        format!(
            "容器已自动创建，距离自动清理还有 {} 分钟",
            idle_timeout_seconds / 60
        )
    } else {
        format!(
            "容器活动时间已刷新，距离自动清理还有 {} 分钟",
            idle_timeout_seconds / 60
        )
    };

    // 转换时间戳为东八区时间字符串
    let previous_activity_time_str = timestamp_to_utc8_string(previous_activity_time);
    let current_activity_time_str = timestamp_to_utc8_string(current_activity_time);

    let response = KeepalivePodResponse {
        existed: !created,
        created,
        container_info: pod_container_info,
        previous_activity_time,
        current_activity_time, // 使用实际数据库更新的时间
        previous_activity_time_str,
        current_activity_time_str,
        time_until_cleanup: idle_timeout_seconds,
        message,
    };

    info!(
        "✅ [POD_KEEPALIVE] 保活完成: existed={}, created={}, time_until_cleanup={}s",
        !created, created, idle_timeout_seconds
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
        (status = 401, description = "API Key 鉴权失败", body = String),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_restart",
    summary = "重启容器（销毁后重建）",
    description = "根据 user_id 和 project_id 重启容器。如果容器存在，先销毁再创建新容器；如果不存在，直接创建。"
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_restart(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RestartPodRequest>,
) -> Result<HttpResult<RestartPodResponse>, AppError> {
    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_RESTART] user_id is required");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id is required",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_RESTART] project_id is required");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id is required",
        ));
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits {
        if let Err(e) = validate_resource_limits(limits) {
 error!("[POD_RESTART] resources message failed: {}", e);
            return Ok(HttpResult::error(
                shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
                &format!("Invalid resource_limits: {}", e),
            ));
        }
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

        // 从 DuckDB 存储中彻底移除旧容器及其所有关联记录
        // 使用 container_id 删除,确保清理该容器关联的所有 project_id
        match state
            .projects
            .delete_container_with_projects(&container_info.container_id)
        {
            Ok((container_deleted, deleted_projects)) => {
                info!(
                    "🧹 [POD_RESTART] 已清理旧容器记录: container_id={}, container_deleted={}, deleted_projects={}",
                    container_info.container_id, container_deleted, deleted_projects
                );
            }
            Err(e) => {
                // 记录错误但继续执行(不阻塞容器创建)
                error!(
                    "⚠️ [POD_RESTART] 清理旧容器记录失败（继续创建）: container_id={}, error={}",
                    container_info.container_id, e
                );
            }
        }

        // 获取 DockerManager 并停止容器
        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                error!("[POD_RESTART] Failed to get DockerManager: {}", e);
                AppError::internal_server_error(&format!("Failed to get DockerManager: {}", e))
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

        // 🆕 使容器 IP 缓存失效（容器名称是稳定的）
        state
            .container_ip_cache
            .invalidate(&container_info.container_name);

        // 🆕 增强逻辑: 验证容器是否真正移除，防止 "Name already in use"
        let container_name = container_info.container_name.clone();
        let mut deletion_confirmed = false;

        for i in 0..10 {
            // 最多等待 5 秒 (10 * 500ms)
            match docker_manager
                .find_container_realtime(&container_name)
                .await
            {
                Ok(Some(_)) => {
                    if i == 0 {
                        info!(
                            "⏳ [POD_RESTART] 容器仍在 Docker 中，等待清理: name={}",
                            container_name
                        );
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Ok(None) => {
                    info!(
                        "✅ [POD_RESTART] 确认容器已从 Docker 移除: name={}",
                        container_name
                    );
                    deletion_confirmed = true;
                    break;
                }
                Err(e) => {
 warn!("[POD_RESTART] checkcontainerremovedstatus message : {}, message alreadyremoved", e);
                    // 如果是其他错误，也可能意味着Container status abnormal，尝试继续
                    deletion_confirmed = true;
                    break;
                }
            }
        }

        if !deletion_confirmed {
            warn!(
                "⚠️ [POD_RESTART] 等待容器移除超时，后续创建可能会失败: name={}",
                container_name
            );
        }
    }

    // 4. 定义资源限制
    let resource_limits = request.resource_limits.map(|limits| ServiceResourceLimits {
        memory_limit: limits.memory,
        cpu_limit: limits.cpu,
        swap_limit: limits.swap,
    });

    // 5. 强制创建新容器
    info!(
        "🏗️ [POD_RESTART] 强制创建新容器: user_id={}",
        request.user_id
    );

    let container_info = ComputerContainerManager::force_create_container_for_user(
        &request.user_id,
        resource_limits,
    )
    .await?;

    info!(
        "✅ [POD_RESTART] 新容器创建成功: container_id={}",
        container_info.container_id
    );

    // 5. 🆕 立即同步 VNC 后端映射
    // 这消除了等待定时同步任务（最多 5 秒）的空窗期
    if let Some(ref pingora_service) = state.pingora_service {
        sync_single_vnc_backend(
            pingora_service,
            &request.user_id,
            &container_info.container_ip,
        )
        .await;
        info!(
            "🔄 [POD_RESTART] VNC 后端映射已同步: user_id={} -> {}",
            request.user_id, container_info.container_ip
        );
    }

    // 6. 在 DuckDB 存储中记录容器信息
    {
        let mut project_info = ProjectAndContainerInfo::new(request.project_id.clone());
        project_info.set_user_id(Some(request.user_id.clone()));
        project_info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        project_info.set_container(Some(container_info.clone()));
        state.insert_project(request.project_id.clone(), Arc::new(project_info));
    }

    // 7. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
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
// 接口五：查询容器状态（是否存活）
// ============================================================================

/// 查询容器状态请求
#[derive(Debug, Clone, Deserialize, IntoParams, ToSchema)]
pub struct PodStatusQuery {
    /// 项目唯一标识符 (可选，user_id 和 project_id 至少需要一个)
    #[param(example = "proj_456")]
    #[schema(example = "proj_456")]
    #[serde(default)]
    pub project_id: Option<String>,

    /// 用户唯一标识符 (可选，user_id 和 project_id 至少需要一个)
    #[param(example = "user_123")]
    #[schema(example = "user_123")]
    #[serde(default)]
    pub user_id: Option<String>,
}

/// 查询容器状态响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodStatusResponse {
    /// 容器是否存活 (true=存在且运行中，false=不存在或未运行)
    #[schema(example = true)]
    pub alive: bool,

    /// 容器状态描述 (running/stopped/not_found)
    #[schema(example = "running")]
    pub status: String,

    /// 容器 ID (如果存在)
    #[schema(example = "abc123def456")]
    pub container_id: Option<String>,

    /// 容器名称 (如果存在)
    #[schema(example = "computer-agent-runner-user_123")]
    pub container_name: Option<String>,

    /// 查询时间戳 (Unix 毫秒)
    #[schema(example = 1702700000000_u64)]
    pub timestamp: u64,

    /// 提示消息
    #[schema(example = "容器正在运行中")]
    pub message: String,
}

/// 查询容器状态（是否存活）
///
/// 根据 user_id 或 project_id 查询对应容器是否存活。
/// 直接查询 Docker API 获取实时状态，无缓存延迟。
///
/// - 如果提供了 user_id，查询 `{container_prefix}-{user_id}` 容器
/// - 如果只提供 project_id，按 project_id 或容器名查询
#[utoipa::path(
    get,
    path = "/computer/pod/status",
    params(
        PodStatusQuery
    ),
    responses(
        (status = 200, description = "成功查询容器状态", body = HttpResult<PodStatusResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = String),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_status",
    summary = "查询容器状态（是否存活）",
    description = "根据 user_id 或 project_id 查询对应容器是否存活"
)]
#[instrument(skip(_state), fields(project_id = ?params.project_id, user_id = ?params.user_id))]
pub async fn pod_status(
    State(_state): State<Arc<AppState>>,
    Query(params): Query<PodStatusQuery>,
) -> Result<HttpResult<PodStatusResponse>, AppError> {
    // 1. 验证参数：至少需要 user_id 或 project_id 之一
    if params.user_id.is_none() && params.project_id.is_none() {
 error!("[POD_STATUS] user_id message project_id message ");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id 和 project_id 至少需要提供一个",
        ));
    }

    info!(
        "🔍 [POD_STATUS] 查询容器状态: project_id={:?}, user_id={:?}",
        params.project_id, params.user_id
    );

    let timestamp = chrono::Utc::now().timestamp_millis() as u64;

    // 2. 获取 DockerManager
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("[POD_STATUS] Failed to get DockerManager: {}", e);
            AppError::internal_server_error(&format!("Failed to get DockerManager: {}", e))
        })?;

    // 3. 查询容器状态
    //
    // 两种查询路径：
    // - user_id 路径：使用 find_user_container()，通过 ServiceConfig 获取配置化的容器前缀
    //   （如 "rcoder-computer-agent-runner-{user_id}"），避免硬编码前缀与实际容器名不一致
    // - project_id 路径：直接使用 find_container_realtime()，project_id 作为 DuckDB 中存储的容器标识符
    let query_result = if let Some(ref user_id) = params.user_id {
        // user_id 路径：通过配置化前缀查找容器
        docker_manager
            .find_user_container(user_id, &shared_types::ServiceType::ComputerAgentRunner)
            .await
    } else if let Some(ref project_id) = params.project_id {
        // project_id 路径：直接按标识符查找（DuckDB 中存储的是完整容器名）
        docker_manager.find_container_realtime(project_id).await
    } else {
        unreachable!()
    };

    // 4. 通过 DockerManager 查询容器状态
    match query_result {
        Ok(Some(result)) => {
            let status_str = if result.is_running {
                "running"
            } else {
                "stopped"
            };
            let message = if result.is_running {
                "容器正在运行中".to_string()
            } else {
                format!("容器存在但状态为: {:?}", result.status)
            };

            info!(
                "✅ [POD_STATUS] 容器状态: alive={}, status={}, container_id={}",
                result.is_running, status_str, result.container_id
            );

            return Ok(HttpResult::success(PodStatusResponse {
                alive: result.is_running,
                status: status_str.to_string(),
                container_id: Some(result.container_id),
                container_name: Some(result.container_name),
                timestamp,
                message,
            }));
        }
        Ok(None) => {
            // 容器不存在，继续尝试 project_id
        }
        Err(e) => {
            error!("[POD_STATUS] Failed to query container status: {}", e);
            return Err(AppError::internal_server_error(&format!(
                "Failed to query container status: {}",
                e
            )));
        }
    }

    // 5. 如果用 user_id 没找到，且同时提供了 project_id，再试 project_id
    if params.user_id.is_some() {
        if let Some(ref project_id) = params.project_id {
            match docker_manager.find_container_realtime(project_id).await {
                Ok(Some(result)) => {
                    let status_str = if result.is_running {
                        "running"
                    } else {
                        "stopped"
                    };
                    let message = if result.is_running {
                        "容器正在运行中".to_string()
                    } else {
                        format!("容器存在但状态为: {:?}", result.status)
                    };

                    info!(
                        "✅ [POD_STATUS] 通过 project_id 找到容器: alive={}, container_id={}",
                        result.is_running, result.container_id
                    );

                    return Ok(HttpResult::success(PodStatusResponse {
                        alive: result.is_running,
                        status: status_str.to_string(),
                        container_id: Some(result.container_id),
                        container_name: Some(result.container_name),
                        timestamp,
                        message,
                    }));
                }
                Ok(None) => {
                    // 容器不存在
                }
                Err(e) => {
 error!("[POD_STATUS] message project_id Query failed: {}", e);
                    // 继续返回 not_found 而不是错误
                }
            }
        }
    }

    // 6. 未找到容器
    info!(
        "📭 [POD_STATUS] 未找到容器: user_id={:?}, project_id={:?}",
        params.user_id, params.project_id
    );

    Ok(HttpResult::success(PodStatusResponse {
        alive: false,
        status: "not_found".to_string(),
        container_id: None,
        container_name: None,
        timestamp,
        message: format!(
            "未找到对应的容器 (user_id={:?}, project_id={:?})",
            params.user_id, params.project_id
        ),
    }))
}

// ============================================================================
// 接口：VNC 状态查询
// ============================================================================

/// VNC 状态查询参数
#[derive(Debug, Clone, Deserialize, IntoParams, ToSchema)]
pub struct VncStatusQuery {
    /// 用户唯一标识符（可选，与 project_id 至少填一个）
    #[param(example = "user_123")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 项目唯一标识符（可选，与 user_id 至少填一个）
    #[param(example = "proj_456")]
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,
}

/// VNC 状态响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VncStatusResponse {
    /// VNC 是否已就绪
    #[schema(example = true)]
    pub vnc_ready: bool,

    /// noVNC 是否已就绪
    #[schema(example = true)]
    pub novnc_ready: bool,

    /// 状态描述消息
    #[schema(example = "VNC 服务已就绪")]
    pub message: String,

    /// 容器启动时长（秒）
    #[schema(example = 120)]
    pub uptime_seconds: i64,

    /// 容器 ID
    #[schema(example = "abc123def456")]
    pub container_id: String,
}

/// 查询容器 VNC 服务状态
///
/// 根据 user_id 或 project_id 定位容器，查询 VNC/noVNC 服务是否已启动就绪。
#[utoipa::path(
    get,
    path = "/computer/pod/vnc-status",
    params(VncStatusQuery),
    responses(
        (status = 200, description = "成功获取 VNC 状态", body = HttpResult<VncStatusResponse>),
        (status = 400, description = "参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = String),
        (status = 404, description = "容器不存在", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_vnc_status",
    summary = "查询容器 VNC 服务状态",
    description = "根据 user_id 或 project_id 定位子容器，查询 VNC/noVNC 服务是否已启动就绪"
)]
#[instrument(skip(state))]
pub async fn pod_vnc_status(
    State(state): State<Arc<AppState>>,
    Query(params): Query<VncStatusQuery>,
) -> Result<HttpResult<VncStatusResponse>, AppError> {
    // 1. 参数验证：user_id 和 project_id 不能同时为空
    let user_id = params.user_id.as_deref().filter(|s| !s.trim().is_empty());
    let project_id = params
        .project_id
        .as_deref()
        .filter(|s| !s.trim().is_empty());

    if user_id.is_none() && project_id.is_none() {
 warn!("[POD_VNC_STATUS] user_id message project_id message empty");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id 和 project_id 不能同时为空",
        ));
    }

    info!(
        "🖥️ [POD_VNC_STATUS] 查询 VNC 状态: user_id={:?}, project_id={:?}",
        user_id, project_id
    );

    // 2. 获取 DockerManager
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("[POD_VNC_STATUS] Failed to get DockerManager: {}", e);
            AppError::internal_server_error(&format!("Failed to get DockerManager: {}", e))
        })?;

    // 3. 定位容器
    // 优先使用 user_id 查找（通过 find_user_container 获取配置化的容器前缀）
    let (lookup_user_id, container_info) = if let Some(uid) = user_id {
        (
            uid,
            docker_manager
                .find_user_container(uid, &shared_types::ServiceType::ComputerAgentRunner)
                .await,
        )
    } else if let Some(pid) = project_id {
        // 如果只有 project_id，通过 DuckDB 查找关联的容器
        if let Some(container_info) = state.projects.get_container_by_user_id(pid) {
            // project_id 可能实际上是 user_id
            (
                pid,
                docker_manager
                    .find_container_realtime(&container_info.container_name)
                    .await,
            )
        } else {
            (pid, Ok(None))
        }
    } else {
        ("", Ok(None))
    };

    let container_info = container_info.map_err(|e| {
        error!("[POD_VNC_STATUS] Failed to query container: {}", e);
        AppError::internal_server_error(&format!("Failed to query container: {}", e))
    })?;

    // 4. 检查容器是否存在
    let result = match container_info {
        Some(info) => info,
        None => {
            info!(
                "📭 [POD_VNC_STATUS] 容器不存在: user_id={:?}, project_id={:?}",
                user_id, project_id
            );
            return Ok(HttpResult::error(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                &format!(
                    "容器不存在: user_id={:?}, project_id={:?}",
                    user_id, project_id
                ),
            ));
        }
    };

    // 5. 检查容器是否正在运行
    if !result.is_running {
        info!(
            "⚠️ [POD_VNC_STATUS] 容器未运行: container_id={}",
            result.container_id
        );
        return Ok(HttpResult::success(VncStatusResponse {
            vnc_ready: false,
            novnc_ready: false,
            message: "容器未运行".to_string(),
            uptime_seconds: 0,
            container_id: result.container_id,
        }));
    }

    // 6. 通过 gRPC 调用容器内的 agent_runner 获取 VNC 状态
    // 使用 get_user_container_info 获取服务 URL
    let agent_info = docker_manager.get_user_container_info(lookup_user_id).await;

    let service_url = match agent_info {
        Ok(Some(info)) => info.service_url,
        _ => {
            // 如果无法获取 agent_info，返回错误
            error!(
                "❌ [POD_VNC_STATUS] 无法获取容器服务信息: container_id={}",
                result.container_id
            );
            return Ok(HttpResult::error(
                shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                "无法获取容器服务地址",
            ));
        }
    };

    // 7. 建立 gRPC 连接并调用 GetVncStatus
    // 使用工具函数安全提取 gRPC 地址（自动处理协议前缀和端口）
    let grpc_addr = extract_grpc_addr_with_port(&service_url, shared_types::GRPC_DEFAULT_PORT)
        .map_err(|e| {
            error!("[POD_VNC_STATUS] Failed to extract gRPC address: {}", e);
            AppError::internal_server_error(&format!("Failed to extract gRPC address: {}", e))
        })?;

 info!("📡 [POD_VNC_STATUS] message gRPC connection: addr={}", grpc_addr);

    match state.grpc_pool.get_client(&grpc_addr).await {
        Ok(mut client) => {
            let grpc_request = shared_types::grpc::GetVncStatusRequest {
                user_id: user_id.map(String::from),
                project_id: project_id.map(String::from),
            };

            match client.get_vnc_status(grpc_request).await {
                Ok(response) => {
                    let resp = response.into_inner();
                    info!(
                        "✅ [POD_VNC_STATUS] gRPC 调用成功: vnc_ready={}, novnc_ready={}",
                        resp.vnc_ready, resp.novnc_ready
                    );

                    Ok(HttpResult::success(VncStatusResponse {
                        vnc_ready: resp.vnc_ready,
                        novnc_ready: resp.novnc_ready,
                        message: resp.message,
                        uptime_seconds: resp.uptime_seconds,
                        container_id: result.container_id,
                    }))
                }
                Err(e) => {
 error!("[POD_VNC_STATUS] gRPC message failed: {}", e);
                    Ok(HttpResult::error(
                        shared_types::error_codes::ERR_GRPC_ERROR,
                        &format!("gRPC 调用失败: {}", e),
                    ))
                }
            }
        }
        Err(e) => {
 error!("[POD_VNC_STATUS] message gRPC connectionfailed: {}", e);
            Ok(HttpResult::error(
                shared_types::error_codes::ERR_GRPC_ERROR,
                &format!("建立 gRPC 连接失败: {}", e),
            ))
        }
    }
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
            memory: Some(4294967296.0),
            cpu: Some(2.0),
            swap: Some(6442450944.0),
        };

        let json = serde_json::to_string(&limits).unwrap();
        assert!(json.contains("4294967296"));
        assert!(json.contains("2.0"));
        assert!(json.contains("6442450944"));
    }

    #[test]
    fn test_ensure_pod_response_serialization() {
        let response = EnsurePodResponse {
            created: true,
            container_info: PodContainerInfo {
                container_id: "abc123".to_string(),
                status: "running".to_string(),
            },
            message: "容器创建成功".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("created"));
        assert!(json.contains("container_info"));
        assert!(json.contains("message"));
    }

    #[test]
    fn test_validate_resource_limits_valid() {
        let limits = PodResourceLimits {
            memory: Some(4294967296.0), // 4GB
            cpu: Some(2.0),
            swap: Some(6442450944.0), // 6GB
        };
        assert!(validate_resource_limits(&limits).is_ok());
    }

    #[test]
    fn test_validate_resource_limits_none_values() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: None,
            swap: None,
        };
        assert!(validate_resource_limits(&limits).is_ok());
    }

    #[test]
    fn test_validate_resource_limits_cpu_zero() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: Some(0.0),
            swap: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_cpu_negative() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: Some(-1.0),
            swap: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_cpu_too_large() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: Some(200.0),
            swap: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_memory_too_small() {
        let limits = PodResourceLimits {
            memory: Some(256_000_000.0), // 256MB
            cpu: None,
            swap: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_memory_too_large() {
        let limits = PodResourceLimits {
            memory: Some(256_000_000_000.0), // 256GB
            cpu: None,
            swap: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_swap_less_than_memory() {
        let limits = PodResourceLimits {
            memory: Some(8_589_934_592.0), // 8GB
            cpu: None,
            swap: Some(4_294_967_296.0), // 4GB
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_swap_too_small() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: None,
            swap: Some(256_000_000.0), // 256MB
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_cpu_boundary() {
        // 测试边界值：0.1 应该失败（小于等于 0）
        let limits = PodResourceLimits {
            memory: None,
            cpu: Some(0.1),
            swap: None,
        };
        assert!(validate_resource_limits(&limits).is_ok());

        // 测试边界值：0.01 应该通过
        let limits = PodResourceLimits {
            memory: None,
            cpu: Some(0.01),
            swap: None,
        };
        assert!(validate_resource_limits(&limits).is_ok());
    }
}
