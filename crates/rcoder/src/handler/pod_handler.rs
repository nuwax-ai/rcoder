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
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, instrument, warn};
use utoipa::{IntoParams, ToSchema};

use super::utils::{I18nJsonOrQuery, I18nQuery};
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
        tracing::warn!("created UTC+8 timezone failed, fallback to UTC+0");
        // east_opt(0) 始终返回 Some(0)，因为 0 是有效参数
        // 使用 unwrap_or_else 避免嵌套 unwrap，仅作为防御性编程
        FixedOffset::east_opt(0).unwrap_or_else(|| {
            // 这个分支永远不会执行，因为 east_opt(0) 不会失败
            unreachable!("FixedOffset::east_opt(0) is guaranteed to return Some")
        })
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
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
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

    /// 容器唯一标识，若传值则使用此 ID 标识容器，实现容器复用
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,
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
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct KeepalivePodRequest {
    /// 用户唯一标识符
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目唯一标识符
    #[schema(example = "proj_456")]
    pub project_id: String,

    // === 新增字段 (多租户隔离支持) ===
    /// 容器唯一标识，若传值则使用此 ID 标识容器
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,
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
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
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

    /// 容器唯一标识，若传值则使用此 ID 标识容器，实现容器复用
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,
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
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_count",
    summary = "获取当前容器数量",
    description = "获取当前运行的容器总数及按服务类型分类的统计"
)]
pub async fn pod_count(
    State(state): State<Arc<AppState>>,
) -> Result<HttpResult<PodCountResponse>, AppError> {
    debug!("📊 [POD_COUNT] Getting container count");

    // 获取全局 Runtime
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| {
            error!("[POD_COUNT] Failed to get runtime: {}", e);
            AppError::internal_server_error(&format!("Failed to get runtime: {}", e))
        })?;

    // 获取所有容器列表
    let containers = runtime.list_containers().await.map_err(|e| {
        error!("[POD_COUNT] Failed to list containers: {}", e);
        AppError::internal_server_error(&format!("Failed to list containers: {}", e))
    })?;

    // 获取容器前缀（从 AppState 获取，启动时已初始化）
    let rcoder_prefix = state.container_prefix_rcoder.as_str();
    let computer_prefix = state.container_prefix_computer.as_str();

    // 按服务类型统计（仅统计运行中的容器）
    let mut rcoder_count = 0u32;
    let mut computer_count = 0u32;

    for container in &containers {
        // 仅统计运行中的容器
        if container.status != container_runtime_api::ContainerRuntimeStatus::Running {
            continue;
        }

        if container.container_name.starts_with(&rcoder_prefix) {
            rcoder_count += 1;
        } else if container.container_name.starts_with(&computer_prefix) {
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
        "✅ [POD_COUNT] Container count completed: total={}, rcoder={}, computer_agent_runner={}",
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
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
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
    I18nQuery(params): I18nQuery<PodListQuery>,
) -> Result<HttpResult<PodListResponse>, AppError> {
    debug!(
        "📋 [POD_LIST] get containers: limit={:?}",
        params.limit
    );

    // 1. 获取 runtime 容器列表
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| {
            error!("[POD_LIST] Failed to get runtime: {}", e);
            AppError::internal_server_error(&format!("Failed to get runtime: {}", e))
        })?;

    let runtime_containers = runtime.list_containers().await.map_err(|e| {
        error!("[POD_LIST] Failed to list runtime containers: {}", e);
        AppError::internal_server_error(&format!("Failed to list runtime containers: {}", e))
    })?;

    // 2. 获取 DuckDB 存储中的容器记录
    let duckdb_containers = state.projects.get_all_container_records().map_err(|e| {
        error!("[POD_LIST] Failed to get DuckDB container list: {}", e);
        AppError::internal_server_error(&format!("Failed to get DuckDB container list: {}", e))
    })?;

    // 3. 获取容器前缀（从 AppState 获取，启动时已初始化）
    let rcoder_prefix = state.container_prefix_rcoder.as_str();
    let computer_prefix = state.container_prefix_computer.as_str();

    // 4. 创建容器ID到DuckDB记录的映射
    let mut duckdb_map: std::collections::HashMap<String, &duckdb_manager::ContainerRecord> =
        std::collections::HashMap::new();
    for record in &duckdb_containers {
        duckdb_map.insert(record.container_id.clone(), record);
    }

    // 5. 合并数据，构建容器详细信息列表
    let mut containers: Vec<PodDetailInfo> = Vec::new();

    for docker_container in &runtime_containers {
        // 仅处理运行中的容器
        if docker_container.status != container_runtime_api::ContainerRuntimeStatus::Running {
            continue;
        }

        let duckdb_record = duckdb_map.get(&docker_container.container_id);

        // 确定服务类型
        let service_type = if docker_container.container_name.starts_with(rcoder_prefix) {
            "RCoder"
        } else if docker_container.container_name.starts_with(computer_prefix) {
            "ComputerAgentRunner"
        } else {
            "Unknown"
        };

        // 从容器名称提取 user_id（如果是 computer-agent-runner-{user_id}）
        let user_id = if docker_container.container_name.starts_with(computer_prefix) {
            docker_container
                .container_name
                .strip_prefix(&format!("{}-", computer_prefix))
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
                if !docker_container.container_name.is_empty() && docker_container.container_name != "unknown" {
                    Some(docker_container.container_name.clone())
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
                .unwrap_or_else(|| docker_container.container_ip.clone()),
            service_url: duckdb_record
                .map(|r| r.service_url.clone())
                .unwrap_or_else(|| format!("http://{}:{}", docker_container.container_ip, 8086)),
            status: String::from(docker_container.status.clone()),
            service_type: service_type.to_string(),
            project_id,
            user_id: final_user_id,
            created_at: docker_container.created_at.timestamp_millis() as u64,
            last_activity: duckdb_record.map(|r| r.last_activity.timestamp_millis() as u64),
            image: None,
            internal_port: Some(8086),
            external_port: duckdb_record.map(|r| r.external_port),
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
        "✅ [POD_LIST] Container list retrieved: total={}, returned={}, paginated={}",
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
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
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
    I18nJsonOrQuery(request): I18nJsonOrQuery<EnsurePodRequest>,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_ENSURE] user_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_ENSURE] project_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits {
        if let Err(e) = validate_resource_limits(limits) {
            error!("[POD_ENSURE] resources update failed: {}", e);
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
                locale,
            ));
        }
    }

    info!(
        "🚀 [POD_ENSURE] Ensuring container exists: user_id={}, project_id={}",
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
                "⏳ [POD_ENSURE] Container is being created, waiting for completion: user_id={}, elapsed={:?}",
                request.user_id, elapsed
            );

            // 轮询等待容器就绪（最多等 30 秒，每秒检查一次）
            let mut waited_container_info = None;
            for wait_sec in 1..=30 {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                // 标记已被移除 = 创建完成
                if !state.pod_creating.contains_key(&request.user_id) {
                    // 尝试获取容器信息
                    if let Ok(runtime) = docker_manager::runtime::RuntimeManager::get().await {
                        if let Ok(Some(info)) = runtime
                            .get_container_info_by_identifier(
                                &request.user_id,
                                &shared_types::ServiceType::ComputerAgentRunner,
                            )
                            .await
                        {
                            info!(
                                "✅ [POD_ENSURE] Wait succeeded, container ready (waited {}s): user_id={}, container_id={}",
                                wait_sec, request.user_id, info.container_id
                            );
                            waited_container_info = Some(info);
                            break;
                        }
                    }
                }

                if wait_sec % 5 == 0 {
                    debug!(
                        "[POD_ENSURE] Still waiting for container creation: user_id={}, {}s",
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
                        "🔄 [POD_ENSURE] VNC backend mapping synced: user_id={} -> {}",
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
                    pinfo.set_pod_id(request.pod_id.clone());
                    pinfo.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
                    pinfo.set_container(Some(info.clone()));
                    pinfo
                };
                state.insert_project(request.project_id.clone(), Arc::new(project_info));
                debug!(
                    "📝 [POD_ENSURE] DuckDB record updated: project_id={}, user_id={}, container_id={}",
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
                        "Container ready (waiting for other request to complete creation): container_id={}",
                        info.container_id
                    ),
                }));
            }
            // 等待超时，继续正常的创建流程（此时标记可能已过期被清理）
            warn!(
                "⚠️ [POD_ENSURE] Wait for container creation timeout (30s), will continue to try creating: user_id={}",
                request.user_id
            );
        } else {
            // 标记过期，清理后继续
            warn!(
                "⚠️ [POD_ENSURE] Creation mark expired ({:?}), cleaning up and continuing",
                elapsed
            );
            state.pod_creating.remove(&request.user_id);
        }
    }

    // 2. 🔍 实时查询 runtime 检查容器是否存在（不依赖缓存）
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| {
            error!("[POD_ENSURE] Failed to get runtime: {}", e);
            AppError::internal_server_error(&format!("Failed to get runtime: {}", e))
        })?;

    let existing_container = runtime
        .find_container(
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
        Some(result) if result.status == container_runtime_api::ContainerRuntimeStatus::Running => {
            // 容器存在且正在运行，无需创建
            info!(
                "📦 [POD_ENSURE] Container already exists and running: container_id={}, status={:?}",
                result.container_id, result.status
            );
            false
        }
        Some(result) => {
            // 容器存在但未运行（Exited 等状态），需要删除并重建
            warn!(
                "⚠️ [POD_ENSURE] Container exists but not running: container_id={}, status={:?}, will delete and recreate",
                result.container_id, result.status
            );

            // 删除旧容器（使用 pod_id 优先的标识符，与创建时一致）
            // 如果删除失败（包括容器不存在等情况），返回错误让调用者知道
            let container_identifier = request.pod_id.as_deref().unwrap_or(&request.user_id);
            runtime
                .stop_container_by_identifier(
                    container_identifier,
                    &shared_types::ServiceType::ComputerAgentRunner,
                )
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
                "✅ [POD_ENSURE] Old container deleted: container_id={}",
                result.container_id
            );

            // 清理旧容器的 gRPC 连接
            if !result.container_ip.is_empty() {
                let old_grpc_addr = format!(
                    "{}:{}",
                    result.container_ip,
                    shared_types::GRPC_DEFAULT_PORT
                );
                state.grpc_pool.remove(&old_grpc_addr);
            }

            // ⏱️ 等待 Docker 完全释放容器资源（避免竞态条件）
            // Docker 删除是异步操作，立即创建同名容器可能导致资源冲突
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            debug!("⏱️ [POD_ENSURE] container resources already released");

            true
        }
        None => {
            // 容器不存在，需要创建
            info!("🏗️ [POD_ENSURE] container not found, will create new container");
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
                request.pod_id.as_deref(),
                request.isolation_type.as_deref(),
                request.tenant_id.as_deref(),
                request.space_id.as_deref(),
            )
            .await
            {
                Ok(info) => {
                    if attempt > 1 {
                        info!(
                            "✅ [POD_ENSURE] Container created successfully (attempt {}): container_id={}",
                            attempt, info.container_id
                        );
                    } else {
                        info!(
                            "✅ [POD_ENSURE] Container created successfully: container_id={}",
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
                            "⚠️ [POD_ENSURE] Container creation failed (attempt {}), will retry: {}",
                            attempt,
                            last_error
                                .as_ref()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "Unknown error".to_string())
                        );
                        // 等待一段时间后重试（指数退避）
                        tokio::time::sleep(tokio::time::Duration::from_millis(
                            200 * attempt as u64,
                        ))
                        .await;
                    } else {
                        error!(
                            "[POD_ENSURE] Container creation failed (already retry {})",
                            max_attempts
                        );
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
                    .unwrap_or_else(|| "Container creation failed but no error info captured".to_string());
                return Err(AppError::internal_server_error(&error_msg));
            }
        }
    } else {
        // 获取现有容器的完整信息
        match runtime
            .get_container_info_by_identifier(
                &request.user_id,
                &shared_types::ServiceType::ComputerAgentRunner,
            )
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
                    "⚠️ [POD_ENSURE] Container running but internal mapping not ready, waiting for sync: user_id={}",
                    request.user_id
                );

                let mut retry_info = None;
                for retry_attempt in 1..=3 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    match runtime
                        .get_container_info_by_identifier(
                            &request.user_id,
                            &shared_types::ServiceType::ComputerAgentRunner,
                        )
                        .await
                    {
                        Ok(Some(info)) => {
                            info!(
                                "✅ [POD_ENSURE] Internal mapping synced (retry {}): container_id={}",
                                retry_attempt, info.container_id
                            );
                            retry_info = Some(info);
                            break;
                        }
                        _ => {
                            debug!(
                                "[POD_ENSURE] Mapping not found: retry {}",
                                retry_attempt
                            );
                        }
                    }
                }

                match retry_info {
                    Some(info) => (info, false),
                    None => {
                        // 3次重试后仍失败，才考虑重建
                        warn!(
                            "⚠️ [POD_ENSURE] Wait for sync timeout, attempting to recreate: user_id={}",
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
                            request.pod_id.as_deref(),
                            request.isolation_type.as_deref(),
                            request.tenant_id.as_deref(),
                            request.space_id.as_deref(),
                        )
                        .await;

                        // 清除创建标记
                        state.pod_creating.remove(&request.user_id);

                        match result {
                            Ok(info) => {
                                info!(
                                    "✅ [POD_ENSURE] Container recreated successfully: container_id={}",
                                    info.container_id
                                );
                                (info, true)
                            }
                            Err(e) => {
                                error!(
                                    "❌ [POD_ENSURE] Container recreation failed: user_id={}, error={}",
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
                    "❌ [POD_ENSURE] Failed to get container full info: user_id={}, error={}",
                    request.user_id, e
                );
                return Err(AppError::internal_server_error(&format!(
                    "Failed to get container full info: {}",
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
                "🔄 [POD_ENSURE] VNC backend mapping synced: user_id={} -> {}",
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
        info.set_pod_id(request.pod_id.clone());
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_container(Some(container_info.clone()));
        info
    };

    state.insert_project(request.project_id.clone(), Arc::new(project_info));
    debug!(
        "📝 [POD_ENSURE] DuckDB record updated: project_id={}, user_id={}, container_id={}",
        request.project_id, request.user_id, container_info.container_id
    );

    // 6. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        status: container_info.status.clone(),
    };

    let message = if created {
        "Container created successfully, can access virtual desktop via VNC (Agent service not started)".to_string()
    } else {
        "Container already exists, can access virtual desktop via VNC directly".to_string()
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
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
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
    I18nJsonOrQuery(request): I18nJsonOrQuery<KeepalivePodRequest>,
) -> Result<HttpResult<KeepalivePodResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_KEEPALIVE] user_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_KEEPALIVE] project_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    // 1.1 验证隔离参数完整性（当 pod_id 有值时）
    let container_identifier = if let Some(ref pod_id) = request.pod_id {
        if request.isolation_type.is_none() || request.tenant_id.is_none() || request.space_id.is_none() {
            error!("[POD_KEEPALIVE] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided");
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
            ));
        }
        // 记录验证通过的参数（此时 pod_id, isolation_type, tenant_id, space_id 必定为 Some）
        if let (Some(it), Some(tid), Some(sid)) = (
            request.isolation_type.as_deref(),
            request.tenant_id.as_deref(),
            request.space_id.as_deref(),
        ) {
            info!(
                "🔒 [POD_KEEPALIVE] Using pod_id for container lookup: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
                pod_id, it, tid, sid
            );
        }
        pod_id.clone()
    } else {
        request.user_id.clone()
    };

    info!(
        "💓 [POD_KEEPALIVE] Container keepalive: user_id={}, project_id={}, container_identifier={}",
        request.user_id, request.project_id, container_identifier
    );

    // 2. 检查 DuckDB 存储中是否有记录，并更新活动时间
    // 更新当前项目的 last_activity；共享容器场景下还需更新同容器下其他项目
    let (previous_activity_time, current_activity_time, existed) = {
        if let Some(existing_info) = state.get_project(&request.project_id) {
            let prev = existing_info.last_activity().timestamp_millis() as u64;

            // 获取实际更新的时间
            let updated_time = state.update_activity(&request.project_id);
            let current = updated_time
                .map(|t| t.timestamp_millis() as u64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as u64);

            // 更新同容器下其他项目的 last_activity（仅限共享容器场景）
            if let Some(ref pod_id) = request.pod_id {
                let related_projects = state.projects.find_projects_by_pod_id(pod_id);
                for related in &related_projects {
                    if related.project_id != request.project_id {
                        state.update_activity(&related.project_id);
                    }
                }
            }
            // 非共享容器模式：每个项目独立，不需更新其他项目

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
            ComputerContainerManager::get_container_info(&container_identifier).await?;

        match existing_container {
            Some(info) => {
                // Docker 中有容器，检查并插入到 DuckDB 存储
                if !state.contains_project(&request.project_id) {
                    let mut project_info = ProjectAndContainerInfo::new(request.project_id.clone());
                    project_info.set_user_id(Some(request.user_id.clone()));
                    project_info.set_pod_id(request.pod_id.clone());
                    project_info
                        .set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
                    project_info.set_container(Some(info.clone()));
                    state.insert_project(request.project_id.clone(), Arc::new(project_info));
                    info!(
                        "[POD_KEEPALIVE] container already exists (Docker), updating DuckDB"
                    );
                } else {
                    info!(
                        "[POD_KEEPALIVE] container already exists (Docker), DuckDB already up to date"
                    );
                }
                (info, false)
            }
            None => {
                // Docker 中也没有容器，返回错误而不是创建新容器
                info!(
                    "❌ [POD_KEEPALIVE] container not found: container_identifier={}",
                    container_identifier
                );
                return Ok(HttpResult::error_with_locale(
                    shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                    locale,
                ));
            }
        }
    } else {
        // DuckDB 存储中有记录，直接获取容器信息
        let info = ComputerContainerManager::get_container_info(&container_identifier)
            .await?
            .ok_or_else(|| {
                error!(
                    "[POD_KEEPALIVE] Container status abnormal: DuckDB has record but container not found in Docker"
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
            "Container auto created, {} minutes until auto cleanup",
            idle_timeout_seconds / 60
        )
    } else {
        format!(
            "Container activity time refreshed, {} minutes until auto cleanup",
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
        "✅ [POD_KEEPALIVE] Keepalive completed: existed={}, created={}, time_until_cleanup={}s",
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
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
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
    I18nJsonOrQuery(request): I18nJsonOrQuery<RestartPodRequest>,
) -> Result<HttpResult<RestartPodResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_RESTART] user_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_RESTART] project_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits {
        if let Err(e) = validate_resource_limits(limits) {
            error!("[POD_RESTART] resources update failed: {}", e);
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
                locale,
            ));
        }
    }

    info!(
        "🔄 [POD_RESTART] Restarting container: user_id={}, project_id={}",
        request.user_id, request.project_id
    );

    // 2. 检查容器是否存在
    let existing_container = ComputerContainerManager::get_container_info(&request.user_id).await?;
    let was_existing = existing_container.is_some();

    // 3. 如果容器存在，先销毁
    if let Some(container_info) = existing_container {
        info!(
            "🗑️ [POD_RESTART] Destroying existing container: container_id={}",
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
                    "🧹 [POD_RESTART] Cleaned up old container records: container_id={}, container_deleted={}, deleted_projects={}",
                    container_info.container_id, container_deleted, deleted_projects
                );
            }
            Err(e) => {
                // 记录错误但继续执行(不阻塞容器创建)
                error!(
                    "⚠️ [POD_RESTART] Failed to clean up old container records (will continue creating): container_id={}, error={}",
                    container_info.container_id, e
                );
            }
        }

        let runtime = docker_manager::runtime::RuntimeManager::get()
            .await
            .map_err(|e| {
                error!("[POD_RESTART] Failed to get runtime: {}", e);
                AppError::internal_server_error(&format!("Failed to get runtime: {}", e))
            })?;

        // 使用 pod_id 优先的标识符停止容器（与创建时一致）
        let container_identifier = request.pod_id.as_deref().unwrap_or(&request.user_id);
        if let Err(e) = runtime
            .stop_container_by_identifier(container_identifier, &shared_types::ServiceType::ComputerAgentRunner)
            .await
        {
            // 记录错误但继续尝试创建新容器
            error!(
                "⚠️ [POD_RESTART] Failed to stop container (will continue creating new container): container_id={}, error={}",
                container_info.container_id, e
            );
        } else {
            info!(
                "✅ [POD_RESTART] Container destroyed: container_id={}",
                container_info.container_id
            );
        }

        // 🆕 清理旧容器的 gRPC 连接（避免复用已失效的 TCP 连接）
        if !container_info.container_ip.is_empty() {
            let old_grpc_addr = format!(
                "{}:{}",
                container_info.container_ip,
                shared_types::GRPC_DEFAULT_PORT
            );
            state.grpc_pool.remove(&old_grpc_addr);
        }

        // 验证容器是否真正移除
        let mut deletion_confirmed = false;

        for i in 0..10 {
            // 最多等待 5 秒 (10 * 500ms)
            match runtime
                .find_container(
                    &request.user_id,
                    &shared_types::ServiceType::ComputerAgentRunner,
                )
                .await
            {
                Ok(Some(_)) => {
                    if i == 0 {
                        info!(
                            "⏳ [POD_RESTART] Container still exists, waiting for cleanup: user_id={}",
                            request.user_id
                        );
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Ok(None) => {
                    info!(
                        "✅ [POD_RESTART] Confirmed container removed: user_id={}",
                        request.user_id
                    );
                    deletion_confirmed = true;
                    break;
                }
                Err(e) => {
                    warn!(
                        "[POD_RESTART] check container removed status: {}, container already removed",
                        e
                    );
                    // 如果是其他错误，也可能意味着Container status abnormal，尝试继续
                    deletion_confirmed = true;
                    break;
                }
            }
        }

        if !deletion_confirmed {
            warn!(
                "⚠️ [POD_RESTART] Wait for container removal timeout, subsequent creation may fail: user_id={}",
                request.user_id
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
        "🏗️ [POD_RESTART] Force creating new container: user_id={}",
        request.user_id
    );

    let container_info = ComputerContainerManager::force_create_container_for_user(
        &request.user_id,
        resource_limits,
        request.pod_id.as_deref(),
        request.isolation_type.as_deref(),
        request.tenant_id.as_deref(),
        request.space_id.as_deref(),
    )
    .await?;

    info!(
        "✅ [POD_RESTART] New container created successfully: container_id={}",
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
            "🔄 [POD_RESTART] VNC backend mapping synced: user_id={} -> {}",
            request.user_id, container_info.container_ip
        );
    }

    // 6. 在 DuckDB 存储中记录容器信息
    {
        let mut project_info = ProjectAndContainerInfo::new(request.project_id.clone());
        project_info.set_user_id(Some(request.user_id.clone()));
        project_info.set_pod_id(request.pod_id.clone());
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
        "Container restarted, can access virtual desktop via VNC (Agent service not started)".to_string()
    } else {
        "Container created (previously did not exist), can access virtual desktop via VNC (Agent service not started)".to_string()
    };

    let response = RestartPodResponse {
        was_existing,
        restarted: true,
        container_info: pod_container_info,
        message,
    };

    info!(
        "✅ [POD_RESTART] Completed: was_existing={}, container_id={}",
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

    // === 新增字段 (多租户隔离支持) ===
    /// 容器唯一标识，若传值则使用此 ID 标识容器
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "pod_tenant_123")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[param(example = "tenant_abc")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[param(example = "space_xyz")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "tenant")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,
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
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
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
    I18nQuery(params): I18nQuery<PodStatusQuery>,
) -> Result<HttpResult<PodStatusResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 验证参数：至少需要 pod_id、user_id 或 project_id 之一
    if params.pod_id.is_none() && params.user_id.is_none() && params.project_id.is_none() {
        error!("[POD_STATUS] pod_id, user_id and project_id are all empty");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    // 1.1 验证隔离参数完整性（当 pod_id 有值时）
    let container_identifier = if let Some(ref pod_id) = params.pod_id {
        if params.isolation_type.is_none() || params.tenant_id.is_none() || params.space_id.is_none() {
            error!("[POD_STATUS] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided");
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
            ));
        }
        // 记录验证通过的参数（此时 pod_id, isolation_type, tenant_id, space_id 必定为 Some）
        if let (Some(it), Some(tid), Some(sid)) = (
            params.isolation_type.as_deref(),
            params.tenant_id.as_deref(),
            params.space_id.as_deref(),
        ) {
            info!(
                "🔒 [POD_STATUS] Using pod_id for container lookup: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
                pod_id, it, tid, sid
            );
        }
        Some(pod_id.clone())
    } else {
        None
    };

    info!(
        "🔍 [POD_STATUS] Querying container status: project_id={:?}, user_id={:?}, pod_id={:?}, container_identifier={:?}",
        params.project_id, params.user_id, params.pod_id, container_identifier
    );

    let timestamp = chrono::Utc::now().timestamp_millis() as u64;

    // 2. 获取 Runtime
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| {
            error!("[POD_STATUS] Failed to get runtime: {}", e);
            AppError::internal_server_error(&format!("Failed to get runtime: {}", e))
        })?;

    // 3. 查询容器状态
    // 优先级：pod_id > user_id > project_id
    let query_result = if let Some(ref identifier) = container_identifier {
        // 使用 pod_id 查找（多租户场景）
        runtime
            .find_container(identifier, &shared_types::ServiceType::ComputerAgentRunner)
            .await
    } else if let Some(ref user_id) = params.user_id {
        runtime
            .find_container(user_id, &shared_types::ServiceType::ComputerAgentRunner)
            .await
    } else if let Some(ref project_id) = params.project_id {
        runtime
            .find_container(project_id, &shared_types::ServiceType::RCoder)
            .await
    } else {
        unreachable!()
    };

    // 4. 通过 runtime 查询容器状态
    match query_result {
        Ok(Some(result)) => {
            let is_running = result.status == container_runtime_api::ContainerRuntimeStatus::Running;
            let status_str = if is_running {
                "running"
            } else {
                "stopped"
            };
            let message = if is_running {
                "container is running".to_string()
            } else {
                format!("container exists but status is: {:?}", result.status)
            };

            info!(
                "✅ [POD_STATUS] Container status: alive={}, status={}, container_id={}",
                is_running, status_str, result.container_id
            );

            return Ok(HttpResult::success(PodStatusResponse {
                alive: is_running,
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
            match runtime
                .find_container(project_id, &shared_types::ServiceType::RCoder)
                .await
            {
                Ok(Some(result)) => {
                    let is_running = result.status == container_runtime_api::ContainerRuntimeStatus::Running;
                    let status_str = if is_running {
                        "running"
                    } else {
                        "stopped"
                    };
                    let message = if is_running {
                        "container is running".to_string()
                    } else {
                        format!("container exists but status is: {:?}", result.status)
                    };

                    info!(
                        "✅ [POD_STATUS] Found container by project_id: alive={}, container_id={}",
                        is_running, result.container_id
                    );

                    return Ok(HttpResult::success(PodStatusResponse {
                        alive: is_running,
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
                    error!("[POD_STATUS] Query failed: {}", e);
                    // 继续返回 not_found 而不是错误
                }
            }
        }
    }

    // 6. 未找到容器
    info!(
        "📭 [POD_STATUS] Container not found: user_id={:?}, project_id={:?}",
        params.user_id, params.project_id
    );

    Ok(HttpResult::success(PodStatusResponse {
        alive: false,
        status: "not_found".to_string(),
        container_id: None,
        container_name: None,
        timestamp,
        message: format!(
            "Container not found (user_id={:?}, project_id={:?})",
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

    // === 新增字段 (多租户隔离支持) ===
    /// 容器唯一标识，若传值则使用此 ID 标识容器
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "pod_tenant_123")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[param(example = "tenant_abc")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "shared_types::flexible_string::flexible_string")]
    #[param(example = "space_xyz")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "tenant")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,
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
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
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
    I18nQuery(params): I18nQuery<VncStatusQuery>,
) -> Result<HttpResult<VncStatusResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 参数验证：pod_id、user_id 和 project_id 不能同时为空
    let user_id = params.user_id.as_deref().filter(|s| !s.trim().is_empty());
    let project_id = params
        .project_id
        .as_deref()
        .filter(|s| !s.trim().is_empty());
    let pod_id = params.pod_id.as_deref().filter(|s| !s.trim().is_empty());

    // 1.1 验证隔离参数完整性（当 pod_id 有值时）
    if pod_id.is_some() {
        if params.isolation_type.is_none() || params.tenant_id.is_none() || params.space_id.is_none() {
            error!("[POD_VNC_STATUS] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided");
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
            ));
        }
    }

    if pod_id.is_none() && user_id.is_none() && project_id.is_none() {
        warn!("[POD_VNC_STATUS] pod_id, user_id and project_id are all empty");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    info!(
        "🖥️ [POD_VNC_STATUS] Querying VNC status: user_id={:?}, project_id={:?}, pod_id={:?}",
        user_id, project_id, pod_id
    );

    // 2. 获取 Runtime
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| {
            error!("[POD_VNC_STATUS] Failed to get runtime: {}", e);
            AppError::internal_server_error(&format!("Failed to get runtime: {}", e))
        })?;

    // 3. 定位容器
    // 优先级：pod_id > user_id > project_id
    let (lookup_user_id, container_info) = if let Some(pid) = pod_id {
        // 使用 pod_id 查找（多租户场景）
        (
            pid,
            runtime
                .find_container(pid, &shared_types::ServiceType::ComputerAgentRunner)
                .await,
        )
    } else if let Some(uid) = user_id {
        (
            uid,
            runtime
                .find_container(uid, &shared_types::ServiceType::ComputerAgentRunner)
                .await,
        )
    } else if let Some(pid) = project_id {
        // 如果只有 project_id，通过 DuckDB 查找关联的容器
        if state.projects.get_container_by_user_id(pid).is_some() {
            // project_id 可能实际上是 user_id
            (
                pid,
                runtime
                    .find_container(
                        pid,
                        &shared_types::ServiceType::ComputerAgentRunner,
                    )
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
                "📭 [POD_VNC_STATUS] Container does not exist: user_id={:?}, project_id={:?}",
                user_id, project_id
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                locale,
            ));
        }
    };

    // 5. 检查容器是否正在运行
    if result.status != container_runtime_api::ContainerRuntimeStatus::Running {
        info!(
            "⚠️ [POD_VNC_STATUS] Container not running: container_id={}",
            result.container_id
        );
        return Ok(HttpResult::success(VncStatusResponse {
            vnc_ready: false,
            novnc_ready: false,
            message: "Container not running".to_string(),
            uptime_seconds: 0,
            container_id: result.container_id,
        }));
    }

    // 6. 构建 gRPC 地址
    // 直接使用步骤 3 find_container 获取的 container_ip，避免二次缓存查找失败
    // （find_container 实时查询 Docker API，get_container_info_by_identifier 只查内存缓存，
    //   服务重启后缓存丢失会导致查找失败）
    let container_ip = if !result.container_ip.is_empty() {
        result.container_ip.clone()
    } else {
        // 缓存命中时 IP 可能为空，重新实时查询获取 IP
        match runtime
            .find_container(lookup_user_id, &shared_types::ServiceType::ComputerAgentRunner)
            .await
        {
            Ok(Some(info)) if !info.container_ip.is_empty() => info.container_ip,
            _ => {
                error!(
                    "❌ [POD_VNC_STATUS] unable to get container IP: container_id={}",
                    result.container_id
                );
                return Ok(HttpResult::error_with_locale(
                    shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                    locale,
                ));
            }
        }
    };

    let grpc_addr = format!("{}:{}", container_ip, shared_types::GRPC_DEFAULT_PORT);

    info!(
        "📡 [POD_VNC_STATUS] Checking gRPC connection: addr={}",
        grpc_addr
    );

    match state.grpc_pool.get_client(&grpc_addr).await {
        Ok(mut client) => {
            let grpc_request = crate::grpc::new_request_with_locale(
                shared_types::grpc::GetVncStatusRequest {
                    user_id: user_id.map(String::from),
                    project_id: project_id.map(String::from),
                },
                locale,
            );

            match client.get_vnc_status(grpc_request).await {
                Ok(response) => {
                    let resp = response.into_inner();
                    info!(
                        "✅ [POD_VNC_STATUS] gRPC call successful: vnc_ready={}, novnc_ready={}",
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
                    error!("[POD_VNC_STATUS] gRPC call failed: {}", e);
                    Ok(HttpResult::error_with_locale(
                        shared_types::error_codes::ERR_GRPC_ERROR,
                        locale,
                    ))
                }
            }
        }
        Err(e) => {
            error!("[POD_VNC_STATUS] gRPC connection failed: {}", e);
            Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_GRPC_ERROR,
                locale,
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
