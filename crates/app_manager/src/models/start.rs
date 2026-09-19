//! start/restart 的部署增强请求与响应 DTO（统一部署+启动入口）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `POST /api/v1/userapp/{app_id}/start|restart` 请求体（全可选——无参数即传统启停语义）。
///
/// 带 `url` 即触发**轻量部署**（容器内下载/校验/解压 → 换 code → 编排启动），
/// 是 Java 直发制品包的统一入口（不经 build）。同步等待边界 = **部署段完成**
/// （制品正确落地 + database SQL 执行；服务启动结果异步可见，readiness 探针
/// 照常摘流）；失败 = 部署段失败（容器侧 error 透传）或等待超时，code/ 现场
/// 不破坏（旧制品 URL 重发即回滚）。
///
/// start 无 `url` 且 app 不存在时**创建空容器**（基础设施形态：PG/ttyd/dbx
/// 常驻 + app-cli idle 等部署，按 app_id 定位）；restart 无 `url` 对
/// 不存在的 app 仍 404（重启语义不创建）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, garde::Validate)]
pub struct StartAppRequest {
    /// Idempotency identity for the complete deployment and configuration intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[garde(length(min = 1, max = 128), pattern(shared_types::IDENTIFIER_RE))]
    pub request_id: Option<String>,
    /// Expected lifecycle; required after explicit application recreation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[garde(skip)]
    pub lifecycle_id: Option<String>,
    /// 制品包下载 URL（workspace 整体包 zip）。给出即触发轻量部署链。
    #[garde(skip)]
    pub url: Option<String>,
    /// 请求版本标记。缺省自动生成并在响应返回；与制品 manifest 身份和内部部署操作 ID 分离。
    #[garde(skip)]
    pub release_id: Option<String>,
    /// 制品 sha256（64 位 ASCII 十六进制，大小写统一）。可选——给出则部署前校验格式、下载后校验一致性，
    /// 缺省跳过校验（信任内网源）。
    #[garde(skip)]
    pub sha256: Option<String>,
    /// 应用环境变量（整段替换语义，与 update 一致）。容器内 app-cli 读取。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[garde(skip)]
    pub env: Option<HashMap<String, String>>,
    /// 闲置回收超时（秒）。0 = 不回收（常驻）。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[garde(skip)]
    pub idle_timeout_seconds: Option<u64>,
    /// 显式 PG 凭据。已有版本化配置时必须与本操作捕获的版本一致；
    /// 首次使用时与部署受理同事务创建配置版本；已保存配置不允许被此字段覆盖。
    /// 配置在迁移/业务启动前应用；失败返回操作失败，不返回带 pg_error 的成功结果。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[garde(skip)]
    pub pg: Option<StartPgCredential>,
    /// 是否在部署 activate 后自动执行包内 database 目录 SQL（根 database/ 先 +
    /// 各子项目 database/，文件名升序；单文件失败仅收集进 `sql_report` 不阻断）。
    /// 缺省 true；false 跳过。仅 url 部署时生效。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[garde(skip)]
    pub auto_execute_sql: Option<bool>,
    /// 部署模式（仅带 url 时生效）：`pod`（缺省）= env 注入 → Recreate 换 Pod 部署；
    /// `hot` = 调容器内 app-cli `/v1/deploy` 原地换应用——不换 Pod、PG/ttyd/dbx
    /// 不断连。hot 的一切前置不满足（app 不存在/不在跑/容器为旧镜像）自动回退
    /// pod 模式。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[garde(skip)]
    pub deploy_mode: Option<DeployMode>,
}

/// 部署模式枚举（非法值 serde 直接 400）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeployMode {
    #[default]
    /// 换 Pod 部署（缺省）：env → ConfigMap → Recreate，新 Pod 下载部署。
    Pod,
    /// 热部署：容器内原地换应用（前置不满足自动回退 Pod）。
    Hot,
}

/// PG 凭据（start/restart 部署时自动对齐）——wire 契约已下沉 `shared_types`
/// （dev 链 file-server-userapp 与 prod 链共用同一形状），此处再导出保持
/// `crate::models::StartPgCredential` 公共路径稳定。
pub use shared_types::StartPgCredential;

/// start/restart 响应（传统启停语义 = runtime 字段；部署增强字段按请求出现）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StartAppResult {
    /// Durable operation for this complete start/restart request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// 运行时信息（状态/访问 URL/端口等，与传统启停响应同构）
    #[serde(flatten)]
    pub runtime: super::response::AppRuntimeInfo,
    /// 本次部署的版本标记（url 部署时必有：显式传入或自动生成）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_id: Option<String>,
    /// PG 对齐结果：None=未请求；Some(true)=一致或已重置；Some(false)=对齐失败
    ///（部署不受影响，`pg_error` 带详情可重试）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_aligned: Option<bool>,
    /// PG 对齐失败详情（pg_aligned=false 时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_error: Option<String>,
    /// 包内 database SQL 自动执行报告（url 部署且 auto_execute_sql 未关时；
    /// executed=成功文件列表，failed=失败详情——失败不阻断部署）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql_report: Option<super::db::DatabaseSqlReport>,
}
