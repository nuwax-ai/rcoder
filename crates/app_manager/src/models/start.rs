//! start/restart 的部署增强请求与响应 DTO（统一部署+启动入口）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `POST /api/v1/userapp/{app_id}/start|restart` 请求体（全可选——无参数即传统启停语义）。
///
/// 带 `url` 即触发**轻量部署**（下载 zip → prepare → activate → 启动），
/// 是 Java 直发制品包的统一入口（不经 build）；失败语义对齐发布链
/// （activate 就绪失败保留旧版本现场 + Failed 状态）。
///
/// start 无 `url` 且 app 不存在时**创建空容器**（基础设施形态：PG/ttyd/dbx
/// 常驻 + app-cli idle 等部署，此形态 `user_id` 必填）；restart 无 `url` 对
/// 不存在的 app 仍 404（重启语义不创建）。
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct StartAppRequest {
    /// 制品包下载 URL（workspace 整体包 zip）。给出即触发轻量部署链。
    pub url: Option<String>,
    /// owner 用户 ID。start 无 url 创建空容器时**必填**（数据卷分区依赖）；
    /// 已存在 app 的启动/部署时可选补记——与 build 同款兜底语义：显式传优先，
    /// 注册 `userapp_metadata` owner，供 `/proxy/userapp/prod/{user_id}/...` URL
    /// 拼接与"我的应用"归属过滤；None=不改动已有记录。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// 发布版本标记（幂等键）。缺省自动生成（`rel-{时间戳}-{随机}`）并在响应返回；
    /// 显式传入时同 id+同内容重复部署幂等命中。
    pub release_id: Option<String>,
    /// 制品 sha256（64 位十六进制小写）。可选——给出则下载后校验一致性，
    /// 缺省跳过校验（信任内网源）。
    pub sha256: Option<String>,
    /// 应用环境变量（整段替换语义，与 update 一致）。容器内 app-cli 读取。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// 闲置回收超时（秒）。0 = 不回收（常驻）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u64>,
    /// PG 凭据对齐：给出则部署完成后自动连接测试（scram），不一致自动重置为该值
    /// （对齐失败不阻断部署，结果见响应 `pg_aligned` 字段，可重试）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg: Option<StartPgCredential>,
    /// 是否在部署 activate 后自动执行包内 database 目录 SQL（根 database/ 先 +
    /// 各子项目 database/，文件名升序；单文件失败仅收集进 `sql_report` 不阻断）。
    /// 缺省 true；false 跳过。仅 url 部署时生效。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_execute_sql: Option<bool>,
    /// 部署模式（仅带 url 时生效）：`pod`（缺省）= env 注入 → Recreate 换 Pod 部署；
    /// `hot` = 调容器内 app-cli `/v1/deploy` 原地换应用——不换 Pod、PG/ttyd/dbx
    /// 不断连。hot 的一切前置不满足（app 不存在/不在跑/容器为旧镜像）自动回退
    /// pod 模式。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deploy_mode: Option<DeployMode>,
}

/// 部署模式枚举（非法值 serde 直接 400）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeployMode {
    #[default]
    /// 换 Pod 部署（缺省）：env → ConfigMap → Recreate，新 Pod 下载部署。
    Pod,
    /// 热部署：容器内原地换应用（前置不满足自动回退 Pod）。
    Hot,
}

/// PG 凭据（start/restart 部署时自动对齐）。
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct StartPgCredential {
    /// PG 账号名（已存在角色；须过 PG 标识符白名单）
    pub username: String,
    /// 目标密码（与开发环境保持一致的值）
    pub password: String,
}

/// start/restart 响应（传统启停语义 = runtime 字段；部署增强字段按请求出现）。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StartAppResult {
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
