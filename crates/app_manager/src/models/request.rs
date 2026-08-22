//! 应用管理请求模型

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::commons::{AppStatus, HealthCheckConfig, PortConfig, ResourceLimits};

/// 创建应用请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateAppRequest {
    /// 应用 ID（可选，外部指定；格式 `app-` + DNS-1123，如 `app-order-svc`；None=自动生成）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// 应用名称
    pub name: String,
    /// 归属用户 ID（部署访问 URL `/proxy/apps/{user_id}/{app_id}/{port}` 的组成段；
    /// 存 userapp_metadata.user_id，"我的应用"过滤/归属校验数据源）
    pub user_id: String,
    /// 容器镜像（可选；完整地址含 registry + 命名空间）。
    ///
    /// **缺省 = 平台默认运行时镜像**（env `RCODER_RUNTIME_IMAGE_DIGEST`，部署层按
    /// 环境注入——测试/生产各一份，与发布链 `ensure_app_runtime` 同源）。当前
    /// userApp 统一单一 app-runtime 镜像，调用方通常无需传；显式传入用于临时
    /// 指定特殊版本（如灰度）。env 未配置且未传入 → ERR_BACKEND_ERROR。
    pub image: Option<String>,
    /// 启动命令
    pub command: Option<Vec<String>>,
    /// 环境变量（存储到 ConfigMap）
    pub env: Option<HashMap<String, String>>,
    /// 敏感信息（存储到 Secret）
    pub secrets: Option<HashMap<String, String>>,
    /// 资源限制
    pub resources: Option<ResourceLimits>,
    /// 端口配置
    pub ports: Option<Vec<PortConfig>>,
    /// 健康检查配置
    pub health_check: Option<HealthCheckConfig>,
    /// 租户 ID（多租户场景）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户场景）
    pub space_id: Option<String>,
    /// 是否参与闲置自动回收（None/Some(true)=可回收=免费用户默认；Some(false)=永不回收=付费/常驻）。
    /// rcoder 持久化为 Deployment 注解 `rcoder.io/recycle-enabled`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recycle_enabled: Option<bool>,
    /// 闲置回收阈值秒数（per-app 覆盖全局 `userapp_recycle.idle_timeout_seconds`）。
    /// rcoder 持久化为注解 `rcoder.io/idle-timeout-seconds`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u64>,
}

/// 查询应用请求
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct QueryAppsRequest {
    /// 页码
    pub page: Option<u32>,
    /// 每页数量
    pub page_size: Option<u32>,
    /// 过滤条件
    pub filters: Option<AppFilters>,
    /// 排序字段
    pub sort_by: Option<String>,
    /// 排序方式
    pub sort_order: Option<SortOrder>,
}

/// 应用过滤条件
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AppFilters {
    /// 按状态过滤
    pub status: Option<Vec<AppStatus>>,
    /// 按名称模糊搜索
    pub name: Option<String>,
    /// 按应用 ID 过滤
    pub app_ids: Option<Vec<String>>,
    /// 创建时间范围
    pub created_at: Option<DateRange>,
}

/// 时间范围
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DateRange {
    /// 起始时间（RFC3339）
    pub start: String,
    /// 结束时间（RFC3339）
    pub end: String,
}

/// 排序方式
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

/// 更新应用请求
///
/// **rcoder 无状态**：不持有旧 desired state，无法做"部分字段保留"。因此本请求语义为
/// **全量替换**——调用方（Java，desired state 的 source of truth）需发送完整新状态。
/// `image` 可选——缺失时用平台默认运行时镜像（env `RCODER_RUNTIME_IMAGE_DIGEST`，
/// 与 create 同源；等于"滚动到当前默认镜像版本"）；`ports`/`health_check` 为整段替换。
/// `tenant_id`/`space_id` 携带以保持资源 label（rcoder 不主动修改租户归属）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateAppRequest {
    /// 应用名称（仅元数据，不影响 K8s 资源命名；rcoder 忽略）
    pub name: Option<String>,
    /// 容器镜像（可选；缺失 = 平台默认运行时镜像 env `RCODER_RUNTIME_IMAGE_DIGEST`，
    /// 与 create 同源。rcoder 无状态不持有旧 desired——缺省语义是"用当前默认"而非
    /// "保留旧值"）
    pub image: Option<String>,
    /// 启动命令
    pub command: Option<Vec<String>>,
    /// 环境变量
    pub env: Option<HashMap<String, String>>,
    /// 敏感信息
    pub secrets: Option<HashMap<String, String>>,
    /// 资源限制
    pub resources: Option<ResourceLimits>,
    /// 端口配置（整段替换）
    pub ports: Option<Vec<PortConfig>>,
    /// 健康检查配置
    pub health_check: Option<HealthCheckConfig>,
    /// 租户 ID（携带以保持 label）
    pub tenant_id: Option<String>,
    /// 空间 ID（携带以保持 label）
    pub space_id: Option<String>,
    /// 是否参与闲置自动回收（None=沿用既有/默认；Some 覆盖）。部分更新时由 rcoder 回填旧值（SSA 保留）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recycle_enabled: Option<bool>,
    /// 闲置回收阈值秒数（部分更新时由 rcoder 回填旧值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u64>,
    /// 乐观锁：传入 `GET /apps/{id}` 返回的 `resource_version`；不匹配 → 409 ERR_CONFLICT。
    /// 不传 = 不校验（向后兼容）。Docker 模式 resource_version=None，忽略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_resource_version: Option<String>,
}

/// 设置闲置回收策略（动态、免重启：只 patch Deployment 注解，不碰 pod template → 不触发 rollout）。
///
/// 供计费侧免费↔付费 tier 变更调用：`recycle_enabled=false`（付费→不回收）/`true`（降级免费→恢复回收）。
/// 比 `UpdateAppRequest` 轻——无需 image、不走全量 SSA。至少需传一个字段（皆 None → ERR_VALIDATION）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecyclePolicyRequest {
    /// 是否参与闲置回收。None=不改；Some(true)=可回收（免费默认）；Some(false)=永不回收（付费/常驻）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recycle_enabled: Option<bool>,
    /// 闲置回收阈值秒数（per-app 覆盖全局）。None=不改/沿用。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u64>,
}

/// 删除应用请求
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct DeleteAppRequest {
    /// 是否同时清空持久存储（默认 `false`：只删计算面，保留数据面）
    #[serde(default)]
    pub purge: Option<bool>,
    /// 乐观锁：传入 `GET /apps/{id}` 返回的 `resource_version`；不匹配 → 409 ERR_CONFLICT。
    /// 不传 = 不校验（向后兼容）。Docker 模式忽略。
    #[serde(default)]
    pub expected_resource_version: Option<String>,
}
