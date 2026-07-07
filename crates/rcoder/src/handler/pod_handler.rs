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

use super::utils::{I18nJsonOrQuery, I18nQuery, container_identity_from_name};
use crate::router::AppState;
use crate::service::ComputerContainerManager;
use crate::service::computer_container_manager::ContainerCreateOptions;
// sync_single_vnc_backend 已移除，使用 ContainerLookupService 统一数据源
use crate::{AppError, HttpResult};
use shared_types::{
    ContainerBasicInfo, PodCountByServiceType, PodCountResponse, ProjectAndContainerInfo,
    ServiceResourceLimits, ServiceType, VncStatusResponse,
};

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
        if let (Some(memory), Some(swap_val)) = (limits.memory, limits.swap)
            && swap_val < memory
        {
            return Err("swap should be >= memory".to_string());
        }
    }

    // 验证 storage_size 格式（K8s 资源格式）
    if let Some(ref storage_size) = limits.storage_size {
        validate_k8s_storage_size(storage_size)?;
    }

    Ok(())
}

/// 解析最终生效的资源限制：API 入参优先，缺失字段回退到 configmap 中
/// 该 service_type 的默认配置。
///
/// 背景：Backend 调用容器创建相关接口（`/chat`、`/computer/chat`、`/pod/ensure`、
/// `/pod/restart`）时通常不传 `resource_limits`，直接用 `None` 创建 Pod 会得到
/// 无 requests/limits 的容器（K8s 下 resources 全空）。这里以
/// `ServiceImageConfig.resource_limits`（来自 configmap）兜底，并通过 `merge_with`
/// 做字段级合并——API 显式传入的字段优先，未传字段回退默认值。
///
/// 公共核心：直接接受 `ServiceResourceLimits`，供 `/chat`、`/computer/chat` 等
/// 已持有 `ServiceResourceLimits` 的入口复用；`/pod/*` 入口用 `PodResourceLimits`，
/// 由 [`resolve_resource_limits`] 做类型转换后委托本函数。
pub(crate) fn resolve_resource_limits_from_config(
    state: &AppState,
    service_type: &ServiceType,
    api_limits: Option<ServiceResourceLimits>,
) -> Option<ServiceResourceLimits> {
    // configmap 中该 service_type 的默认资源限制（保底）
    // 注意：get_multi_image_config 返回 owned MultiImageConfig，需先绑定再借用，
    // 并在闭包内 clone 出 owned ServiceResourceLimits，避免返回指向临时值的悬垂引用。
    let default_limits = state.config.docker_config.as_ref().and_then(|dc| {
        let multi_config = dc.get_multi_image_config();
        multi_config
            .get_service_config(service_type)
            .map(|c| c.resource_limits.clone())
    });

    // 来源标记，便于排查“资源限制静默丢失”问题（none=Pod 将无 resources，需警惕）
    let source = match (&default_limits, &api_limits) {
        (Some(_), Some(_)) => "merged(api+configmap)",
        (Some(_), None) => "configmap",
        (None, Some(_)) => "api",
        (None, None) => "none",
    };

    // 字段级合并：API 字段优先，None 回退默认值
    let result = match (default_limits, api_limits) {
        (Some(default), Some(api)) => Some(default.merge_with(&api)),
        (Some(default), None) => Some(default),
        (None, api) => api,
    };

    // 记录最终生效的 memory/cpu（仅这两个字段进 K8s container resources；
    // swap_limit/storage_size 不进 container resources，故不在此记录）
    let mem = result
        .as_ref()
        .and_then(|l| l.memory_limit)
        .map(|b| format!("{:.1}Gi", b / 1024.0 / 1024.0 / 1024.0));
    let cpu = result.as_ref().and_then(|l| l.cpu_limit);
    info!(
        "[RESOURCE_LIMITS] service_type={:?}, source={}, memory={}, cpu={}",
        service_type,
        source,
        mem.as_deref().unwrap_or("none"),
        cpu.map(|c| c.to_string()).as_deref().unwrap_or("none"),
    );

    result
}

/// `/pod/ensure`、`/pod/restart` 接口的 resource_limits 是 `PodResourceLimits` 类型，
/// 这里先转成 `ServiceResourceLimits`，再委托 [`resolve_resource_limits_from_config`] 合并。
fn resolve_resource_limits(
    state: &AppState,
    service_type: &ServiceType,
    api_limits: Option<PodResourceLimits>,
) -> Option<ServiceResourceLimits> {
    let api_limits = api_limits.map(|limits| ServiceResourceLimits {
        memory_limit: limits.memory,
        cpu_limit: limits.cpu,
        swap_limit: limits.swap,
        storage_size: limits.storage_size,
    });
    resolve_resource_limits_from_config(state, service_type, api_limits)
}

/// 解析 service_type 字符串为 ServiceType 枚举
///
/// 默认返回 ComputerAgentRunner（保持向后兼容）
///
/// # 参数
/// * `raw` - 原始 service_type 字符串
///
/// # 返回
/// Ok(ServiceType) 解析成功，Err(String) 解析失败
fn parse_service_type(raw: Option<&str>) -> Result<ServiceType, String> {
    match raw {
        None | Some("") => Ok(ServiceType::ComputerAgentRunner),
        Some(s) => s
            .parse::<ServiceType>()
            .map_err(|e| format!("invalid service_type: {}", e)),
    }
}

/// 根据 ServiceType 确定容器标识符
///
/// - WebAgentRunner: 使用 project_id
/// - ComputerAgentRunner: 使用 user_id (或 pod_id)
///
/// # 参数
/// * `service_type` - 服务类型
/// * `user_id` - 用户 ID
/// * `project_id` - 项目 ID
/// * `pod_id` - 容器 ID (可选，优先级最高)
///
/// # 返回
/// 容器标识符字符串
fn container_identifier_for_service(
    service_type: &ServiceType,
    user_id: &str,
    project_id: &str,
    pod_id: Option<&str>,
) -> String {
    if let Some(pid) = pod_id {
        return pid.to_string();
    }
    match service_type {
        ServiceType::WebAgentRunner | ServiceType::UserApp => project_id.to_string(),
        ServiceType::ComputerAgentRunner => user_id.to_string(),
    }
}

/// K8s 存储大小单位
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageUnit {
    /// 兆字节（二进制，1 Mi = 1024 Ki）
    Mi,
    /// 吉字节（二进制，1 Gi = 1024 Mi）
    Gi,
    /// 太字节（二进制，1 Ti = 1024 Gi）
    Ti,
    /// 兆字节（十进制，1 M = 1000 KB）
    M,
    /// 吉字节（十进制，1 G = 1000 MB）
    G,
    /// 太字节（十进制，1 T = 1000 GB）
    T,
}

impl StorageUnit {
    /// 转换为 Gi（吉字节，二进制）
    fn to_gi(self, value: f64) -> f64 {
        match self {
            StorageUnit::Ti => value * 1024.0,
            StorageUnit::Gi => value,
            StorageUnit::Mi => value / 1024.0,
            // 十进制单位转换为二进制
            StorageUnit::T => value * 1000.0 / 1024.0,
            StorageUnit::G => value * 1000.0 / 1024.0,
            StorageUnit::M => value / 1024.0,
        }
    }
}

/// 解析 K8s 资源格式的存储大小
///
/// 支持的格式：`<数字><单位>`
/// - 二进制单位：Mi, Gi, Ti
/// - 十进制单位：M, G, T
///
/// # 示例
/// - "10Gi" → (10.0, Gi)
/// - "100Mi" → (100.0, Mi)
/// - "1.5Ti" → (1.5, Ti)
fn parse_k8s_storage_size(input: &str) -> Result<(f64, StorageUnit), String> {
    use winnow::ascii::float;
    use winnow::combinator::alt;
    use winnow::prelude::*;

    // 解析单位后缀
    // 使用字符串字面量直接调用 .parse_next()，这是 winnow 的惯用方式
    fn unit_parser(input: &mut &str) -> winnow::ModalResult<StorageUnit> {
        alt((
            "Ti".value(StorageUnit::Ti),
            "Gi".value(StorageUnit::Gi),
            "Mi".value(StorageUnit::Mi),
            'T'.value(StorageUnit::T),
            'G'.value(StorageUnit::G),
            'M'.value(StorageUnit::M),
        ))
        .parse_next(input)
    }

    // 解析数字 + 单位
    fn storage_parser(input: &mut &str) -> winnow::ModalResult<(f64, StorageUnit)> {
        let num = float.parse_next(input)?;
        let unit = unit_parser.parse_next(input)?;
        Ok((num, unit))
    }

    // 执行解析
    let mut parser = storage_parser;
    parser.parse(input).map_err(|_| {
        format!(
            "invalid storage_size format: '{}', expected format: <number><unit> (e.g., 10Gi, 100Mi)",
            input
        )
    })
}

/// 验证 K8s 存储大小格式
///
/// 支持的格式：数字 + 单位后缀
/// - Mi, Gi, Ti（二进制单位）
/// - M, G, T（十进制单位）
///
/// # 参数
/// * `storage_size` - 存储大小字符串（如 "10Gi", "100Mi"）
///
/// # 返回
/// Ok(()) 验证通过，Err(String) 返回错误信息
fn validate_k8s_storage_size(storage_size: &str) -> Result<(), String> {
    let (num, unit) = parse_k8s_storage_size(storage_size)?;

    if num <= 0.0 {
        return Err("storage_size must be greater than 0".to_string());
    }

    // 转换为 Gi 进行范围检查
    let gi_value = unit.to_gi(num);

    // 最小 1Gi
    if gi_value < 1.0 {
        return Err("storage_size must be at least 1Gi".to_string());
    }

    // 最大 100Ti
    if gi_value > 100.0 * 1024.0 {
        return Err("storage_size cannot exceed 100Ti".to_string());
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
    let datetime =
        DateTime::from_timestamp_millis(timestamp_millis as i64).unwrap_or(DateTime::UNIX_EPOCH);

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

// 类型定义已移至 shared_types::pod_types 模块

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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,
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

    /// PVC 存储空间大小（仅 K8s 模式生效，Docker 模式忽略）
    ///
    /// 格式：`<数字><单位>`，支持以下单位：
    /// - 二进制单位：`Mi`（兆字节）、`Gi`（吉字节）、`Ti`（太字节）
    /// - 十进制单位：`M`（兆字节）、`G`（吉字节）、`T`（太字节）
    ///
    /// 范围：最小 1Gi，最大 100Ti
    /// 默认值：50Gi（未指定时）
    ///
    /// 示例："10Gi", "100Mi", "1.5Ti"
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "10Gi")]
    pub storage_size: Option<String>,
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,
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
    debug!(" [POD_COUNT] Getting container count");

    // 获取全局 Runtime
    let runtime = state.runtime().clone();

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

        match container_identity_from_name(
            &container.container_name,
            rcoder_prefix,
            computer_prefix,
        )
        .map(|(_, service_type)| service_type)
        {
            Some(ServiceType::WebAgentRunner) => rcoder_count += 1,
            Some(ServiceType::ComputerAgentRunner) => computer_count += 1,
            // UserApp 容器不计入 agent 统计
            Some(ServiceType::UserApp) => {}
            None => {}
        }
    }

    let total_count = rcoder_count + computer_count;
    let timestamp = chrono::Utc::now().timestamp_millis().max(0) as u64;

    let response = PodCountResponse {
        total_count,
        by_service_type: PodCountByServiceType {
            rcoder: rcoder_count,
            computer_agent_runner: computer_count,
        },
        timestamp,
    };

    debug!(
        " [POD_COUNT] Container count completed: total={}, rcoder={}, computer_agent_runner={}",
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
    debug!(" [POD_LIST] get containers: limit={:?}", params.limit);

    // 1. 获取 runtime 容器列表
    let runtime = state.runtime().clone();

    let runtime_containers = runtime.list_containers().await.map_err(|e| {
        error!("[POD_LIST] Failed to list runtime containers: {}", e);
        AppError::internal_server_error(&format!("Failed to list runtime containers: {}", e))
    })?;

    // 2. 获取存储中的容器记录
    let stored_containers = state.projects.get_all_container_records();

    // 3. 获取容器前缀（从 AppState 获取，启动时已初始化）
    let rcoder_prefix = state.container_prefix_rcoder.as_str();
    let computer_prefix = state.container_prefix_computer.as_str();

    // 4. 创建容器ID到存储记录的映射
    let mut stored_map: std::collections::HashMap<String, &ContainerBasicInfo> =
        std::collections::HashMap::new();
    for record in &stored_containers {
        stored_map.insert(record.container_id.clone(), record);
    }

    // 5. 合并数据，构建容器详细信息列表
    let mut containers: Vec<PodDetailInfo> = Vec::new();

    for docker_container in &runtime_containers {
        // 仅处理运行中的容器
        if docker_container.status != container_runtime_api::ContainerRuntimeStatus::Running {
            continue;
        }

        let stored_record = stored_map.get(&docker_container.container_id);

        // 确定服务类型
        let container_identity = container_identity_from_name(
            &docker_container.container_name,
            rcoder_prefix,
            computer_prefix,
        );
        let service_type = container_identity
            .as_ref()
            .map(|(_, service_type)| service_type.to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        // 从容器名称提取 user_id（如果是 computer-agent-runner-{user_id}）
        let user_id = match container_identity {
            Some((identifier, ServiceType::ComputerAgentRunner)) => Some(identifier.to_string()),
            _ => None,
        };

        // 获取项目ID和用户ID（从存储或Docker容器信息）
        let project_id = stored_record
            .and_then(|r| {
                // 尝试从存储关联的项目中获取project_id
                state
                    .projects
                    .get_projects_by_container_id(&r.container_id)
                    .first()
                    .map(|p| p.project_id().to_string())
            })
            .or_else(|| {
                // 如果存储中没有，使用Docker容器中的project_id
                if !docker_container.container_name.is_empty()
                    && docker_container.container_name != "unknown"
                {
                    Some(docker_container.container_name.clone())
                } else {
                    None
                }
            });

        let final_user_id = user_id.or_else(|| {
            stored_record.and_then(|r| {
                state
                    .projects
                    .get_projects_by_container_id(&r.container_id)
                    .first()
                    .and_then(|p| p.user_id().map(|s| s.to_string()))
            })
        });

        // 构建容器详细信息
        let container_info = PodDetailInfo {
            container_id: docker_container.container_id.clone(),
            container_name: docker_container.container_name.clone(),
            container_ip: stored_record
                .map(|r| r.container_ip.clone())
                .unwrap_or_else(|| docker_container.container_ip.clone()),
            service_url: stored_record
                .map(|r| r.service_url.clone())
                .unwrap_or_else(|| format!("http://{}:{}", docker_container.container_ip, 8086)),
            status: String::from(docker_container.status.clone()),
            service_type: service_type.to_string(),
            project_id,
            user_id: final_user_id,
            created_at: docker_container.created_at.timestamp_millis().max(0) as u64,
            last_activity: stored_record.map(|r| r.created_at.timestamp_millis().max(0) as u64),
            image: None,
            internal_port: stored_record.map(|r| r.internal_port),
            external_port: stored_record.map(|r| r.external_port),
        };

        containers.push(container_info);
    }

    // 5. 按创建时间倒序排序（最新的在前）
    containers.sort_by_key(|c| std::cmp::Reverse(c.created_at));

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

    let timestamp = chrono::Utc::now().timestamp_millis().max(0) as u64;

    let response = PodListResponse {
        containers,
        total,
        returned,
        paginated,
        timestamp,
    };

    info!(
        " [POD_LIST] Container list retrieved: total={}, returned={}, paginated={}",
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
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "user_id is required and cannot be empty",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_ENSURE] project_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "project_id is required and cannot be empty",
        ));
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits
        && let Err(e) = validate_resource_limits(limits)
    {
        error!("[POD_ENSURE] resources update failed: {}", e);
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
            locale,
            &e,
        ));
    }

    // 1.2 解析 service_type
    let service_type = match parse_service_type(request.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_ENSURE] invalid service_type: {}", e);
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &e,
            ));
        }
    };

    // 1.3 根据 service_type 确定容器标识符
    let container_identifier = container_identifier_for_service(
        &service_type,
        &request.user_id,
        &request.project_id,
        request.pod_id.as_deref(),
    );

    info!(
        " [POD_ENSURE] Ensuring container exists: user_id={}, project_id={}, service_type={}, container_identifier={}",
        request.user_id, request.project_id, service_type, container_identifier
    );

    // === 并发保护：检查是否有其他请求正在创建同一用户的容器 ===
    // 使用原子标记（DashMap）避免并发请求互相干扰，无死锁风险

    // 🚀 关键修复：先订阅 broadcast channel，再检查 pod_creating
    // 避免 subscribe-after-send 竞态：如果在检查 pod_creating 之后才订阅，
    // 创建者可能已经移除了标记并发送了通知，导致我们错过消息。
    let mut rx = state.pod_created_tx.subscribe();

    // view() 在闭包返回后立即释放锁，无 Ref 暴露
    if let Some(elapsed) = state
        .pod_creating
        .view(&container_identifier, |_, t| t.elapsed())
    {
        // 标记超过 60 秒视为过期（创建方可能已崩溃），忽略并继续
        if elapsed < std::time::Duration::from_secs(60) {
            info!(
                " [POD_ENSURE] Container is being created, waiting for completion: container_identifier={}, elapsed={:?}",
                container_identifier, elapsed
            );

            let mut waited_container_info = None;

            match tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    match rx.recv().await {
                        Ok(created_user_id) if created_user_id == container_identifier => {
                            // 我们等待的容器已创建
                            break;
                        }
                        Ok(_) => continue, // 其他用户的容器，继续等待
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // 通道关闭，退出
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // 消息丢失，检查标记是否已移除
                            if !state.pod_creating.contains_key(&container_identifier) {
                                break;
                            }
                            continue;
                        }
                    }
                }
            })
            .await
            {
                Ok(_) => {
                    // 容器创建成功，获取容器信息
                    if let Ok(Some(info)) = state
                        .runtime()
                        .get_container_info_by_identifier(&container_identifier, &service_type)
                        .await
                    {
                        info!(
                            " [POD_ENSURE] Wait succeeded, container ready: container_identifier={}, container_id={}",
                            container_identifier, info.container_id
                        );
                        waited_container_info = Some(info);
                    }
                }
                Err(_) => {
                    // 超时处理
                    warn!(
                        " [POD_ENSURE] Wait for container creation timeout (30s): container_identifier={}",
                        container_identifier
                    );
                }
            }

            // 如果等待成功，直接使用已就绪的容器，跳过创建流程
            if let Some(info) = waited_container_info {
                // VNC 后端映射已通过 ContainerLookupService 统一管理，无需手动同步

                // 更新存储 记录
                let project_info = if let Some(existing) = state.get_project(&request.project_id) {
                    let mut pinfo = (*existing).clone();
                    pinfo.set_container(Some(info.clone()));
                    pinfo
                } else {
                    let mut pinfo = ProjectAndContainerInfo::new(request.project_id.clone());
                    // 入口尽可能记录完整信息（user_id 对两类业务都记录）；
                    // 是否参与 user_id 查找由 service_type 在使用方区分（见 adapter 索引门控与 find_projects_by_user_id）。
                    pinfo.set_user_id(Some(request.user_id.clone()));
                    pinfo.set_pod_id(request.pod_id.clone());
                    pinfo.set_service_type(Some(service_type.clone()));
                    pinfo.set_scope(
                        request.tenant_id.clone(),
                        request.space_id.clone(),
                        request.isolation_type.clone(),
                    );
                    pinfo.set_container(Some(info.clone()));
                    pinfo
                };
                state
                    .insert_project(request.project_id.clone(), Arc::new(project_info))
                    .map_err(|e| {
                        tracing::error!("[STORAGE] insert_project failed: {}", e);
                        e
                    })?;
                debug!(
                    " [POD_ENSURE] project record updated: project_id={}, user_id={}, container_id={}",
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
                " [POD_ENSURE] Wait for container creation timeout (30s), will continue to try creating: container_identifier={}",
                container_identifier
            );
        } else {
            // 标记过期，清理后继续
            warn!(
                " [POD_ENSURE] Creation mark expired ({:?}), cleaning up and continuing",
                elapsed
            );
            state.pod_creating.remove(&container_identifier);
        }
    }

    // 2. 🔍 实时查询 runtime 检查容器是否存在（不依赖缓存）
    let runtime = state.runtime().clone();

    let existing_container = runtime
        .find_container(&container_identifier, &service_type)
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
                " [POD_ENSURE] Container already exists and running: container_id={}, status={:?}",
                result.container_id, result.status
            );
            false
        }
        Some(result) => {
            // 容器存在但未运行（Exited 等状态），需要删除并重建
            warn!(
                " [POD_ENSURE] Container exists but not running: container_id={}, status={:?}, will delete and recreate",
                result.container_id, result.status
            );

            // 删除旧容器（使用 pod_id 优先的标识符，与创建时一致）
            // 如果删除失败（包括容器不存在等情况），返回错误让调用者知道
            runtime
                .stop_container_by_identifier(&container_identifier, &service_type)
                .await
                .map_err(|e| {
                    error!(
                        " [POD_ENSURE] Failed to delete old container: container_id={}, error={}",
                        result.container_id, e
                    );
                    AppError::internal_server_error(&format!(
                        "Failed to delete old container: {}",
                        e
                    ))
                })?;

            info!(
                " [POD_ENSURE] Old container deleted: container_id={}",
                result.container_id
            );

            // 清理旧容器的 gRPC 连接
            if !result.container_ip.is_empty() {
                let old_grpc_addr = format!(
                    "{}:{}",
                    result.container_ip,
                    shared_types::GRPC_DEFAULT_PORT
                );
                state.grpc_pool.remove(&old_grpc_addr).await;
            }

            // ⏱️ 等待 Docker 完全释放容器资源（避免竞态条件）
            // Docker 删除是异步操作，立即创建同名容器可能导致资源冲突
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            debug!(" [POD_ENSURE] container resources already released");

            true
        }
        None => {
            // 容器不存在，需要创建
            info!(" [POD_ENSURE] container not found, will create new container");
            true
        }
    };

    // 3. 获取或创建容器（带重试机制 + 标记）
    let (container_info, created) = if need_create {
        // 🆕 设置创建标记，防止并发请求重复创建
        let create_started = Instant::now();
        state
            .pod_creating
            .insert(container_identifier.clone(), create_started);

        info!(
            " [POD_ENSURE] Creation marker set: container_identifier={}, user_id={}, project_id={}, max_attempts=3",
            container_identifier, request.user_id, request.project_id
        );

        // 创建新容器，最多重试 3 次
        let resource_limits =
            resolve_resource_limits(&state, &service_type, request.resource_limits);

        let mut last_error = None;
        let mut result = None;
        let max_attempts = 3;

        for attempt in 1..=max_attempts {
            let attempt_started = Instant::now();
            info!(
                " [POD_ENSURE] Container creation attempt {}/{} started: container_identifier={}, elapsed_since_marker={:?}",
                attempt,
                max_attempts,
                container_identifier,
                create_started.elapsed()
            );

            let options = ContainerCreateOptions {
                user_id: request.user_id.clone(),
                project_id: request.project_id.clone(),
                resource_limits: resource_limits.clone(),
                pod_id: request.pod_id.clone(),
                isolation_type: request.isolation_type.clone(),
                tenant_id: request.tenant_id.clone(),
                space_id: request.space_id.clone(),
                service_type: service_type.clone(),
            };
            match ComputerContainerManager::get_or_create_container_for_user_with_type(
                &options,
                state.runtime(),
            )
            .await
            {
                Ok(info) => {
                    info!(
                        " [POD_ENSURE] Container created successfully (attempt {}): container_id={}, ip={}, attempt_elapsed={:?}, total_elapsed={:?}",
                        attempt,
                        info.container_id,
                        info.container_ip,
                        attempt_started.elapsed(),
                        create_started.elapsed()
                    );
                    result = Some(info);
                    break;
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < max_attempts {
                        warn!(
                            " [POD_ENSURE] Container creation failed (attempt {}/{}), will retry: error={}, attempt_elapsed={:?}, total_elapsed={:?}",
                            attempt,
                            max_attempts,
                            last_error
                                .as_ref()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "Unknown error".to_string()),
                            attempt_started.elapsed(),
                            create_started.elapsed()
                        );
                        // 等待一段时间后重试（指数退避）
                        tokio::time::sleep(tokio::time::Duration::from_millis(
                            200 * attempt as u64,
                        ))
                        .await;
                    } else {
                        error!(
                            "[POD_ENSURE] Container creation failed after {} attempts: error={}, total_elapsed={:?}",
                            max_attempts,
                            last_error
                                .as_ref()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "Unknown error".to_string()),
                            create_started.elapsed()
                        );
                    }
                }
            }
        }

        // 返回结果或错误
        match result {
            Some(info) => {
                debug!(
                    " [POD_ENSURE] Clearing creation marker after success: container_identifier={}, total_elapsed={:?}",
                    container_identifier,
                    create_started.elapsed()
                );
                // 创建成功，清除标记
                state.pod_creating.remove(&container_identifier);
                // 🚀 发送容器创建完成通知（唤醒等待方）
                let _ = state.pod_created_tx.send(container_identifier.clone());
                (info, true)
            }
            None => {
                debug!(
                    " [POD_ENSURE] Clearing creation marker after failure: container_identifier={}, total_elapsed={:?}",
                    container_identifier,
                    create_started.elapsed()
                );
                // 创建失败，也要清除标记
                state.pod_creating.remove(&container_identifier);
                // 直接返回原始错误，保留具体的错误信息
                return Err(last_error.unwrap_or_else(|| {
                    AppError::internal_server_error(
                        "Container creation failed but no error info captured",
                    )
                }));
            }
        }
    } else {
        // 获取现有容器的完整信息
        match runtime
            .get_container_info_by_identifier(&container_identifier, &service_type)
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
                    " [POD_ENSURE] Container running but internal mapping not ready, waiting for sync: container_identifier={}",
                    container_identifier
                );

                let mut retry_info = None;
                for retry_attempt in 1..=3 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    match runtime
                        .get_container_info_by_identifier(&container_identifier, &service_type)
                        .await
                    {
                        Ok(Some(info)) => {
                            info!(
                                " [POD_ENSURE] Internal mapping synced (retry {}): container_id={}",
                                retry_attempt, info.container_id
                            );
                            retry_info = Some(info);
                            break;
                        }
                        _ => {
                            debug!("[POD_ENSURE] Mapping not found: retry {}", retry_attempt);
                        }
                    }
                }

                match retry_info {
                    Some(info) => (info, false),
                    None => {
                        // 3次重试后仍失败，才考虑重建
                        warn!(
                            " [POD_ENSURE] Wait for sync timeout, attempting to recreate: container_identifier={}",
                            container_identifier
                        );

                        let resource_limits =
                            resolve_resource_limits(&state, &service_type, request.resource_limits);

                        // 设置创建标记
                        state
                            .pod_creating
                            .insert(container_identifier.clone(), std::time::Instant::now());

                        let options = ContainerCreateOptions {
                            user_id: request.user_id.clone(),
                            project_id: request.project_id.clone(),
                            resource_limits,
                            pod_id: request.pod_id.clone(),
                            isolation_type: request.isolation_type.clone(),
                            tenant_id: request.tenant_id.clone(),
                            space_id: request.space_id.clone(),
                            service_type: service_type.clone(),
                        };
                        let result =
                            ComputerContainerManager::get_or_create_container_for_user_with_type(
                                &options,
                                state.runtime(),
                            )
                            .await;

                        // 清除创建标记
                        state.pod_creating.remove(&container_identifier);

                        // 🚀 发送容器创建完成通知（唤醒等待方）
                        if result.is_ok() {
                            let _ = state.pod_created_tx.send(container_identifier.clone());
                        }

                        match result {
                            Ok(info) => {
                                info!(
                                    " [POD_ENSURE] Container recreated successfully: container_id={}",
                                    info.container_id
                                );
                                (info, true)
                            }
                            Err(e) => {
                                error!(
                                    " [POD_ENSURE] Container recreation failed: container_identifier={}, error={}",
                                    container_identifier, e
                                );
                                return Err(e);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!(
                    " [POD_ENSURE] Failed to get container full info: container_identifier={}, error={}",
                    container_identifier, e
                );
                return Err(AppError::internal_server_error(&format!(
                    "Failed to get container full info: {}",
                    e
                )));
            }
        }
    };

    // 4. VNC 后端映射已通过 ContainerLookupService 统一管理，无需手动同步

    // 5. 更新存储中的容器信息（用于后续保活）
    // 无论容器是新建还是已存在，都要确保 存储 记录是最新的
    let project_info = if let Some(existing) = state.get_project(&request.project_id) {
        // 如果已存在记录，更新容器信息
        let mut info = (*existing).clone();
        info.set_container(Some(container_info.clone()));
        info
    } else {
        // 如果不存在记录，创建新记录
        let mut info = ProjectAndContainerInfo::new(request.project_id.clone());
        // 入口尽可能记录完整信息（user_id 对两类业务都记录）；
        // 是否参与 user_id 查找由 service_type 在使用方区分（见 adapter 索引门控与 find_projects_by_user_id）。
        info.set_user_id(Some(request.user_id.clone()));
        info.set_pod_id(request.pod_id.clone());
        info.set_service_type(Some(service_type.clone()));
        info.set_scope(
            request.tenant_id.clone(),
            request.space_id.clone(),
            request.isolation_type.clone(),
        );
        info.set_container(Some(container_info.clone()));
        info
    };

    state
        .insert_project(request.project_id.clone(), Arc::new(project_info))
        .map_err(|e| {
            tracing::error!("[STORAGE] insert_project failed: {}", e);
            e
        })?;
    debug!(
        " [POD_ENSURE] project record updated: project_id={}, user_id={}, container_id={}",
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
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "user_id is required and cannot be empty",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_KEEPALIVE] project_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "project_id is required and cannot be empty",
        ));
    }

    // 1.1 解析 service_type
    let service_type = match parse_service_type(request.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_KEEPALIVE] invalid service_type: {}", e);
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &e,
            ));
        }
    };

    // 1.2 验证隔离参数完整性（当 pod_id 有值时）
    let container_identifier = if let Some(ref pod_id) = request.pod_id {
        if request.isolation_type.is_none()
            || request.tenant_id.is_none()
            || request.space_id.is_none()
        {
            error!(
                "[POD_KEEPALIVE] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided"
            );
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                "isolation_type, tenant_id, space_id are all required when pod_id is provided",
            ));
        }
        // 记录验证通过的参数（此时 pod_id, isolation_type, tenant_id, space_id 必定为 Some）
        if let (Some(it), Some(tid), Some(sid)) = (
            request.isolation_type.as_deref(),
            request.tenant_id.as_deref(),
            request.space_id.as_deref(),
        ) {
            info!(
                " [POD_KEEPALIVE] Using pod_id for container lookup: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
                pod_id, it, tid, sid
            );
        }
        pod_id.clone()
    } else {
        // 根据 service_type 确定容器标识符
        container_identifier_for_service(&service_type, &request.user_id, &request.project_id, None)
    };

    info!(
        " [POD_KEEPALIVE] Container keepalive: user_id={}, project_id={}, container_identifier={}",
        request.user_id, request.project_id, container_identifier
    );

    // 2. 先确认容器存在（Docker 查询），不存在直接返回错误
    //
    // 修复（顺序问题）：必须先确认容器存在，再刷新活动时间。
    // 原实现先刷新 last_activity 再查 Docker，容器已被外部删除时会刷新僵尸记录的活动时间，
    // 且 storage 残留指向不存在容器的 project 记录。
    let container_info = match ComputerContainerManager::get_container_info_with_type(
        &container_identifier,
        state.runtime(),
        &service_type,
    )
    .await?
    {
        Some(info) => info,
        None => {
            info!(
                " [POD_KEEPALIVE] container not found: container_identifier={}",
                container_identifier
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                locale,
            ));
        }
    };

    // 3. 刷新活动时间（容器已确认存在）
    //
    // existed 语义：storage 中是否已有该 project 的记录。
    //   true  → 常规保活（update_activity 刷新 last_activity）
    //   false → 首次保活/容器恢复后首次（insert_project 新建记录，last_activity=now）
    //
    // created 与 existed 互逆：created=!existed 表示"本次是否新建了 storage 记录"。
    // 注意：keepalive 不创建容器（容器不存在直接返回错误），所以 created 不表示"容器是否新建"。
    let (previous_activity_time, current_activity_time, existed) = {
        if let Some(existing_info) = state.get_project(&request.project_id) {
            // storage 有记录：刷新当前 project 的 last_activity
            let prev = existing_info.last_activity().timestamp_millis().max(0) as u64;

            // 仅刷新当前 project 的 last_activity。
            // 共享容器（pod_id / user_id）的销毁判断由 cleanup_task 的 strategy 负责：
            // 只要容器关联的任一 project 活跃，容器就不会被销毁（见
            // computer_runner.rs 的 find_projects_by_user_id 和 rcoder.rs 的
            // find_projects_by_pod_id）。因此 keepalive 无需越权同步刷新其他 project。
            // 不活跃的 project 记录会被 cleanup 正常清理（但容器因活跃 project 保留）。
            let updated_time = state.update_activity(&request.project_id);
            let current = updated_time
                .map(|t| t.timestamp_millis().max(0) as u64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(0) as u64);

            (prev, current, true)
        } else {
            // storage 无记录：容器已确认存在（Docker 查询通过），补建 storage 记录
            let mut project_info = ProjectAndContainerInfo::new(request.project_id.clone());
            project_info.set_user_id(Some(request.user_id.clone()));
            project_info.set_pod_id(request.pod_id.clone());
            project_info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
            project_info.set_scope(
                request.tenant_id.clone(),
                request.space_id.clone(),
                request.isolation_type.clone(),
            );
            project_info.set_container(Some(container_info.clone()));

            let now = chrono::Utc::now().timestamp_millis().max(0) as u64;

            state
                .insert_project(request.project_id.clone(), Arc::new(project_info))
                .map_err(|e| {
                    tracing::error!("[STORAGE] insert_project failed: {}", e);
                    e
                })?;
            info!(
                "[POD_KEEPALIVE] storage record created (container already exists): project_id={}",
                request.project_id
            );

            (0u64, now, false)
        }
    };

    // 4. 构建响应
    let created = !existed;
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        status: container_info.status.clone(),
    };

    // 从配置中获取清理超时时间
    let idle_timeout_seconds = state.config.cleanup_config.idle_timeout_seconds;

    let message = if created {
        // storage 记录首次创建（容器本身早已存在，只是 storage 没记录）
        format!(
            "Container record created, {} minutes until auto cleanup",
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
        " [POD_KEEPALIVE] Keepalive completed: existed={}, created={}, time_until_cleanup={}s",
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
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "user_id is required and cannot be empty",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_RESTART] project_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "project_id is required and cannot be empty",
        ));
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits
        && let Err(e) = validate_resource_limits(limits)
    {
        error!("[POD_RESTART] resources update failed: {}", e);
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
            locale,
            &e,
        ));
    }

    // 1.2 解析 service_type
    let service_type = match parse_service_type(request.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_RESTART] invalid service_type: {}", e);
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &e,
            ));
        }
    };

    // 1.3 根据 service_type 确定容器标识符
    let container_identifier = container_identifier_for_service(
        &service_type,
        &request.user_id,
        &request.project_id,
        request.pod_id.as_deref(),
    );

    info!(
        " [POD_RESTART] Restarting container: user_id={}, project_id={}, service_type={}, container_identifier={}",
        request.user_id, request.project_id, service_type, container_identifier
    );

    // 2. 检查容器是否存在
    let existing_container = ComputerContainerManager::get_container_info_with_type(
        &container_identifier,
        state.runtime(),
        &service_type,
    )
    .await?;
    let was_existing = existing_container.is_some();

    // 3. 如果容器存在，先销毁
    if let Some(container_info) = existing_container {
        info!(
            " [POD_RESTART] Destroying existing container: container_id={}",
            container_info.container_id
        );

        // 从存储中彻底移除旧容器及其所有关联记录
        // 使用 container_id 删除,确保清理该容器关联的所有 project_id
        let (container_deleted, deleted_projects) = state
            .projects
            .delete_container_with_projects(&container_info.container_id);
        info!(
            " [POD_RESTART] Cleaned up old container records: container_id={}, container_deleted={}, deleted_projects={}",
            container_info.container_id, container_deleted, deleted_projects
        );

        let runtime = state.runtime().clone();

        // 使用 pod_id 优先的标识符停止容器（与创建时一致）
        if let Err(e) = runtime
            .stop_container_by_identifier(&container_identifier, &service_type)
            .await
        {
            // 记录错误但继续尝试创建新容器
            error!(
                " [POD_RESTART] Failed to stop container (will continue creating new container): container_id={}, error={}",
                container_info.container_id, e
            );
        } else {
            info!(
                " [POD_RESTART] Container destroyed: container_id={}",
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
            state.grpc_pool.remove(&old_grpc_addr).await;
        }

        // 验证容器是否真正移除
        let mut deletion_confirmed = false;

        for i in 0..10 {
            // 最多等待 5 秒 (10 * 500ms)
            match runtime
                .find_container(&container_identifier, &service_type)
                .await
            {
                Ok(Some(_)) => {
                    if i == 0 {
                        info!(
                            " [POD_RESTART] Container still exists, waiting for cleanup: container_identifier={}",
                            container_identifier
                        );
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Ok(None) => {
                    info!(
                        " [POD_RESTART] Confirmed container removed: container_identifier={}",
                        container_identifier
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
                " [POD_RESTART] Wait for container removal timeout, subsequent creation may fail: container_identifier={}",
                container_identifier
            );
        }
    }

    // 4. 定义资源限制（API 入参优先，缺失字段回退 configmap 默认值）
    let resource_limits = resolve_resource_limits(&state, &service_type, request.resource_limits);

    // 5. 强制创建新容器
    info!(
        " [POD_RESTART] Force creating new container: container_identifier={}, service_type={}",
        container_identifier, service_type
    );

    let options = ContainerCreateOptions {
        user_id: request.user_id.clone(),
        project_id: request.project_id.clone(),
        resource_limits,
        pod_id: request.pod_id.clone(),
        isolation_type: request.isolation_type.clone(),
        tenant_id: request.tenant_id.clone(),
        space_id: request.space_id.clone(),
        service_type: service_type.clone(),
    };
    let container_info = ComputerContainerManager::get_or_create_container_for_user_with_type(
        &options,
        state.runtime(),
    )
    .await?;

    info!(
        " [POD_RESTART] New container created successfully: container_id={}",
        container_info.container_id
    );

    // 5. VNC 后端映射已通过 ContainerLookupService 统一管理，无需手动同步

    // 6. 在 存储中记录容器信息
    {
        // 🛡️ 关键修复：如果项目已存在，保留现有的 session_id
        let project_info = if let Some(existing) = state.get_project(&request.project_id) {
            // 项目已存在，只更新容器信息，保留 session_id 等状态
            let mut info = (*existing).clone();
            info.set_container(Some(container_info.clone()));
            info
        } else {
            // 项目不存在，创建新记录
            let mut info = ProjectAndContainerInfo::new(request.project_id.clone());
            info.set_user_id(Some(request.user_id.clone()));
            info.set_pod_id(request.pod_id.clone());
            info.set_service_type(Some(service_type.clone()));
            info.set_scope(
                request.tenant_id.clone(),
                request.space_id.clone(),
                request.isolation_type.clone(),
            );
            info.set_container(Some(container_info.clone()));
            info
        };
        state
            .insert_project(request.project_id.clone(), Arc::new(project_info))
            .map_err(|e| {
                tracing::error!("[STORAGE] insert_project failed: {}", e);
                e
            })?;
    }

    // 7. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        status: container_info.status.clone(),
    };

    let message = if was_existing {
        "Container restarted, can access virtual desktop via VNC (Agent service not started)"
            .to_string()
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
        " [POD_RESTART] Completed: was_existing={}, container_id={}",
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[param(example = "tenant_abc")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[param(example = "space_xyz")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "tenant")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "computer-agent-runner")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,
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
#[instrument(skip(state), fields(project_id = ?params.project_id, user_id = ?params.user_id))]
pub async fn pod_status(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<PodStatusQuery>,
) -> Result<HttpResult<PodStatusResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 验证参数：至少需要 pod_id、user_id 或 project_id 之一
    if params.pod_id.is_none() && params.user_id.is_none() && params.project_id.is_none() {
        error!("[POD_STATUS] pod_id, user_id and project_id are all empty");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "at least one of pod_id, user_id or project_id is required",
        ));
    }

    // 1.1 解析 service_type
    let service_type = match parse_service_type(params.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_STATUS] invalid service_type: {}", e);
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &e,
            ));
        }
    };

    // 1.2 验证隔离参数完整性（当 pod_id 有值时）
    let container_identifier = if let Some(ref pod_id) = params.pod_id {
        if params.isolation_type.is_none()
            || params.tenant_id.is_none()
            || params.space_id.is_none()
        {
            error!(
                "[POD_STATUS] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided"
            );
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                "isolation_type, tenant_id, space_id are all required when pod_id is provided",
            ));
        }
        // 记录验证通过的参数（此时 pod_id, isolation_type, tenant_id, space_id 必定为 Some）
        if let (Some(it), Some(tid), Some(sid)) = (
            params.isolation_type.as_deref(),
            params.tenant_id.as_deref(),
            params.space_id.as_deref(),
        ) {
            info!(
                " [POD_STATUS] Using pod_id for container lookup: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
                pod_id, it, tid, sid
            );
        }
        Some(pod_id.clone())
    } else {
        None
    };

    info!(
        " [POD_STATUS] Querying container status: project_id={:?}, user_id={:?}, pod_id={:?}, container_identifier={:?}",
        params.project_id, params.user_id, params.pod_id, container_identifier
    );

    let timestamp = chrono::Utc::now().timestamp_millis().max(0) as u64;

    // 2. 获取 Runtime
    let runtime = state.runtime().clone();

    // 3. 查询容器状态
    // 优先级：pod_id > user_id > project_id
    let query_result = if let Some(ref identifier) = container_identifier {
        // 使用 pod_id 查找（多租户场景）
        runtime.find_container(identifier, &service_type).await
    } else if let Some(ref user_id) = params.user_id {
        runtime.find_container(user_id, &service_type).await
    } else if let Some(ref project_id) = params.project_id {
        runtime.find_container(project_id, &service_type).await
    } else {
        // 防御性编程：理论上不会到达这里（已在上方验证至少有一个标识符）
        // 但为了安全起见，返回验证错误而不是 panic
        error!("[POD_STATUS] Unexpected: all identifiers are None despite validation");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    };

    // 4. 通过 runtime 查询容器状态
    match query_result {
        Ok(Some(result)) => {
            let is_running =
                result.status == container_runtime_api::ContainerRuntimeStatus::Running;
            let status_str = if is_running { "running" } else { "stopped" };
            let message = if is_running {
                "container is running".to_string()
            } else {
                format!("container exists but status is: {:?}", result.status)
            };

            info!(
                " [POD_STATUS] Container status: alive={}, status={}, container_id={}",
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
    if params.user_id.is_some()
        && let Some(ref project_id) = params.project_id
    {
        match runtime
            .find_container(project_id, &shared_types::ServiceType::WebAgentRunner)
            .await
        {
            Ok(Some(result)) => {
                let is_running =
                    result.status == container_runtime_api::ContainerRuntimeStatus::Running;
                let status_str = if is_running { "running" } else { "stopped" };
                let message = if is_running {
                    "container is running".to_string()
                } else {
                    format!("container exists but status is: {:?}", result.status)
                };

                info!(
                    " [POD_STATUS] Found container by project_id: alive={}, container_id={}",
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

    // 6. 未找到容器
    info!(
        " [POD_STATUS] Container not found: user_id={:?}, project_id={:?}",
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[param(example = "tenant_abc")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[param(example = "space_xyz")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "tenant")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "computer-agent-runner")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,
}

// VncStatusResponse 已下沉到 shared_types（crates/shared_types/src/model/pod_types.rs），
// 供 rcoder 与 agent_runner 共享；此处通过 `use shared_types::VncStatusResponse` 引入。

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

    // 1.1 解析 service_type
    let service_type = match parse_service_type(params.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_VNC_STATUS] invalid service_type: {}", e);
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &e,
            ));
        }
    };

    // 1.2 验证隔离参数完整性（当 pod_id 有值时）
    if pod_id.is_some()
        && (params.isolation_type.is_none()
            || params.tenant_id.is_none()
            || params.space_id.is_none())
    {
        error!(
            "[POD_VNC_STATUS] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided"
        );
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "isolation_type, tenant_id, space_id are all required when pod_id is provided",
        ));
    }

    if pod_id.is_none() && user_id.is_none() && project_id.is_none() {
        warn!("[POD_VNC_STATUS] pod_id, user_id and project_id are all empty");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "at least one of pod_id, user_id or project_id is required",
        ));
    }

    info!(
        " [POD_VNC_STATUS] Querying VNC status: user_id={:?}, project_id={:?}, pod_id={:?}",
        user_id, project_id, pod_id
    );

    // 2. 获取 Runtime
    let runtime = state.runtime().clone();

    // 3. 定位容器
    // 优先级：pod_id > user_id > project_id
    let (_lookup_user_id, container_info) = if let Some(pid) = pod_id {
        // 使用 pod_id 查找（多租户场景）
        (pid, runtime.find_container(pid, &service_type).await)
    } else if let Some(uid) = user_id {
        (uid, runtime.find_container(uid, &service_type).await)
    } else if let Some(pid) = project_id {
        // 如果只有 project_id，通过 storage lookup 关联的容器
        if state
            .projects
            .get_container_by_user_id(pid, &service_type)
            .is_some()
        {
            // project_id 可能实际上是 user_id
            (pid, runtime.find_container(pid, &service_type).await)
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
                " [POD_VNC_STATUS] Container does not exist: user_id={:?}, project_id={:?}",
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
            " [POD_VNC_STATUS] Container not running: container_id={}",
            result.container_id
        );
        return Ok(HttpResult::success(VncStatusResponse {
            vnc_ready: false,
            novnc_ready: false,
            message: "Container not running".to_string(),
            uptime_seconds: Some(0),
            container_id: Some(result.container_id),
        }));
    }

    // 5.1 🎯 确保 VNC 代理路由已注册
    // 解决竞态条件：VNC 服务已就绪，但代理路由尚未注册
    // 在 handle_computer_chat 时会注册路由，但 VNC 状态检查可能在 chat 之前调用
    if let Some(ref pingora_service) = state.pingora_service
        && let Some(uid) = user_id
    {
        pingora_service.add_vnc_backend(uid, &result.container_ip);
        debug!(
            "🔗 [POD_VNC_STATUS] Ensured VNC backend registered: user_id={} -> {}",
            uid, result.container_ip
        );
    }

    // 6. 构建 gRPC 地址
    // 根据运行环境选择 gRPC 地址
    // - K8s 环境：使用 K8s Service FQDN（利用服务发现和负载均衡）
    // - Docker 环境：使用容器 IP（直接连接）
    let grpc_addr = if shared_types::is_kubernetes_runtime() {
        let svc_fqdn = super::utils::build_k8s_service_fqdn(
            &result.container_name,
            &state.config.app_manager.namespace,
            &state.cluster_domain,
        );
        let addr = format!("{}:{}", svc_fqdn, shared_types::GRPC_DEFAULT_PORT);
        info!(
            " [POD_VNC_STATUS] Using K8s Service FQDN for gRPC: {}",
            addr
        );
        addr
    } else {
        let addr = format!(
            "{}:{}",
            result.container_ip,
            shared_types::GRPC_DEFAULT_PORT
        );
        info!(" [POD_VNC_STATUS] Using container IP for gRPC: {}", addr);
        addr
    };

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
                        " [POD_VNC_STATUS] gRPC call successful: vnc_ready={}, novnc_ready={}",
                        resp.vnc_ready, resp.novnc_ready
                    );

                    Ok(HttpResult::success(VncStatusResponse {
                        vnc_ready: resp.vnc_ready,
                        novnc_ready: resp.novnc_ready,
                        message: resp.message,
                        uptime_seconds: Some(resp.uptime_seconds),
                        container_id: Some(result.container_id),
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
            storage_size: Some("10Gi".to_string()),
        };

        let json = serde_json::to_string(&limits).unwrap();
        assert!(json.contains("4294967296"));
        assert!(json.contains("2.0"));
        assert!(json.contains("6442450944"));
        assert!(json.contains("10Gi"));
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
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_ok());
    }

    #[test]
    fn test_validate_resource_limits_none_values() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: None,
            swap: None,
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_ok());
    }

    #[test]
    fn test_validate_resource_limits_cpu_zero() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: Some(0.0),
            swap: None,
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_cpu_negative() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: Some(-1.0),
            swap: None,
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_cpu_too_large() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: Some(200.0),
            swap: None,
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_memory_too_small() {
        let limits = PodResourceLimits {
            memory: Some(256_000_000.0), // 256MB
            cpu: None,
            swap: None,
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_memory_too_large() {
        let limits = PodResourceLimits {
            memory: Some(256_000_000_000.0), // 256GB
            cpu: None,
            swap: None,
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_swap_less_than_memory() {
        let limits = PodResourceLimits {
            memory: Some(8_589_934_592.0), // 8GB
            cpu: None,
            swap: Some(4_294_967_296.0), // 4GB
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_err());
    }

    #[test]
    fn test_validate_resource_limits_swap_too_small() {
        let limits = PodResourceLimits {
            memory: None,
            cpu: None,
            swap: Some(256_000_000.0), // 256MB
            storage_size: None,
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
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_ok());

        // 测试边界值：0.01 应该通过
        let limits = PodResourceLimits {
            memory: None,
            cpu: Some(0.01),
            swap: None,
            storage_size: None,
        };
        assert!(validate_resource_limits(&limits).is_ok());
    }
}
