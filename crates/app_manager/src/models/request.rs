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
    /// 归属用户 ID（部署访问 URL `/proxy/userapp/prod/{user_id}/{app_id}` 的组成段；
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, garde::Validate)]
pub struct QueryAppsRequest {
    /// 归属用户 ID（必填——按 metadata owner 过滤"我的应用"；无归属记录的应用不返回）
    #[garde(pattern(shared_types::IDENTIFIER_RE))]
    pub user_id: String,
    /// 页码
    #[garde(skip)]
    pub page: Option<u32>,
    /// 每页数量
    #[garde(skip)]
    pub page_size: Option<u32>,
    /// 过滤条件
    #[garde(skip)]
    pub filters: Option<AppFilters>,
    /// 排序字段
    #[garde(skip)]
    pub sort_by: Option<String>,
    /// 排序方式
    #[garde(skip)]
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
/// **可更新面 = 元数据/资源面**：`env`/`secrets`/`resources`（None=沿用 live 值回退，
/// 显式传=整段替换）+ `recycle`/`idle`/`tenant`/`space`/乐观锁。
/// **command/ports/health_check 不可更新**——v2 四要素平台内定（启动命令=manifest 自动、
/// HTTP 入口=pingap 9080 唯一、探针=app-cli 3010），update 恒从 live 容器 spec 读回，
/// 防调用方误传破坏发布链内定值。`image` 可选——缺失=滚动到平台默认运行时镜像
/// （env `RCODER_RUNTIME_IMAGE_DIGEST`）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, garde::Validate)]
pub struct UpdateAppRequest {
    /// 宿主机数据卷分区归属目录名（必填；Docker compose 挂载路径组成段，
    /// 容器未启动时按此自动唤醒后挂载）
    #[garde(pattern(shared_types::IDENTIFIER_RE))]
    pub user_id: String,
    /// 应用名称（仅元数据，不影响 K8s 资源命名；rcoder 忽略）
    #[garde(skip)]
    pub name: Option<String>,
    /// 容器镜像（可选；缺失 = 平台默认运行时镜像 env `RCODER_RUNTIME_IMAGE_DIGEST`，
    /// 与 create 同源。rcoder 无状态不持有旧 desired——缺省语义是"用当前默认"而非
    /// "保留旧值"）
    #[garde(skip)]
    pub image: Option<String>,
    /// 环境变量（None=沿用 live 值；显式传=整段替换，与 start 部署语义一致）
    #[garde(skip)]
    pub env: Option<HashMap<String, String>>,
    /// 敏感信息（None=沿用 live 值；显式传=整段替换）
    #[garde(skip)]
    pub secrets: Option<HashMap<String, String>>,
    /// 资源限制（None=沿用 live 值）
    #[garde(skip)]
    pub resources: Option<ResourceLimits>,
    /// 租户 ID（携带以保持 label）
    #[garde(skip)]
    pub tenant_id: Option<String>,
    /// 空间 ID（携带以保持 label）
    #[garde(skip)]
    pub space_id: Option<String>,
    /// 是否参与闲置自动回收（None=沿用既有/默认；Some 覆盖）。部分更新时由 rcoder 回填旧值（SSA 保留）。
    #[garde(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recycle_enabled: Option<bool>,
    /// 闲置回收阈值秒数（部分更新时由 rcoder 回填旧值）。
    #[garde(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u64>,
    /// 乐观锁：传入 `GET /apps/{id}` 返回的 `resource_version`；不匹配 → 409 ERR_CONFLICT。
    /// 不传 = 不校验（向后兼容）。Docker 模式 resource_version=None，忽略。
    #[garde(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_resource_version: Option<String>,
}

/// 设置闲置回收策略（动态、免重启：只 patch Deployment 注解，不碰 pod template → 不触发 rollout）。
///
/// 供计费侧免费↔付费 tier 变更调用：`recycle_enabled=false`（付费→不回收）/`true`（降级免费→恢复回收）。
/// 比 `UpdateAppRequest` 轻——无需 image、不走全量 SSA。至少需传一个字段（皆 None → ERR_VALIDATION）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, garde::Validate)]
pub struct RecyclePolicyRequest {
    /// 宿主机数据卷分区归属目录名（Docker compose 形态挂载路径
    /// `prod/{user_id}/data/{app_id}` 的组成段）：容器未启动时按
    /// user_id+app_id+app_stage 自动唤醒后挂载，策略落点才有效
    #[garde(pattern(shared_types::IDENTIFIER_RE))]
    pub user_id: String,
    /// 是否参与闲置回收。None=不改；Some(true)=可回收（免费默认）；Some(false)=永不回收（付费/常驻）。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[garde(skip)]
    pub recycle_enabled: Option<bool>,
    /// 闲置回收阈值秒数（per-app 覆盖全局）。None=不改/沿用。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[garde(skip)]
    pub idle_timeout_seconds: Option<u64>,
    /// scale-to-zero（stop）期间是否允许流量自动唤醒。None=不改；Some 覆盖
    /// `rcoder.io/wake-on-traffic` 注解（与 recycle_enabled 同族的计费 tier 动态开关：
    /// 付费常驻=false 不唤醒 / 免费可回收=true 流量唤醒）。响应字段
    /// `AppRuntimeInfo.wake_on_traffic` 回读。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[garde(skip)]
    pub wake_on_traffic: Option<bool>,
}

/// 删除应用请求
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, garde::Validate)]
pub struct DeleteAppRequest {
    /// 是否同时清空持久存储（默认 `false`：只删计算面，保留数据面）
    #[garde(skip)]
    #[serde(default)]
    pub purge: Option<bool>,
    /// 宿主机数据卷分区归属目录名（必填；标识符白名单校验）——purge 时按
    /// `prod/{user_id}/data/{app_id}` 精确定位宿主目录；缺省回退
    /// userapp_metadata.owner 的兜底路径退役。
    #[garde(pattern(shared_types::IDENTIFIER_RE))]
    pub user_id: String,
    /// 乐观锁：传入 `GET /apps/{id}` 返回的 `resource_version`；不匹配 → 409 ERR_CONFLICT。
    /// 不传 = 不校验（向后兼容）。Docker 模式忽略。
    #[garde(skip)]
    #[serde(default)]
    pub expected_resource_version: Option<String>,
}
