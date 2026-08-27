//! build / project 域请求体与 Query 参数。
//!
//! 字段为 `pub`（models 是 crate 内公共层）；serde 属性、garde 校验与
//! 字段 doc comment 是 wire 契约的一部分，改动须同批核查守卫测试。

use garde::Validate;
use serde::Deserialize;

use super::code::{FileEntry, FileOp};

// ── build 域 ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParseErrorBody {
    #[allow(dead_code)]
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub project_id: String,
    pub error_message: String,
}

/// 多 handler 共用的项目查询参数 (start/stop/restart/build)。
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct BuildQuery {
    /// 项目 ID（workspace 根目录名）
    pub project_id: String,
    /// UserApp 开发卷定位 (可选): 传 appId 时 workspace 走 UserApp 开发卷
    /// (`{USERAPP_WORKSPACE_DIR}/{appId}`), 与 projectId 定位二选一。
    #[serde(default)]
    pub app_id: Option<String>,
    /// 关联进程 PID（可选；build 启动后回传，keep-alive 场景使用）
    #[serde(default)]
    pub pid: Option<String>,
    /// 项目内子路径 (可选；限定文件操作的基准目录)
    #[serde(default)]
    pub base_path: Option<String>,
    /// 租户 ID（多租户隔离，透传给 ProjectContext；本地部署可缺省）
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离，透传给 ProjectContext；本地部署可缺省）
    #[serde(default)]
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离，透传给 ProjectContext；本地部署可缺省）
    #[serde(default)]
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct DevLogQuery {
    /// 项目 ID
    pub project_id: String,
    /// 日志起始行号 (1 起; 缺省 1 即从头读取)
    #[serde(default = "default_start_index")]
    pub start_index: usize,
    /// 日志类型: `temp`(运行日志,默认) / `app`(应用自定义日志)
    #[serde(default = "default_log_type")]
    pub log_type: String,
}
fn default_start_index() -> usize {
    1
}
fn default_log_type() -> String {
    "temp".to_string()
}

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[garde(allow_unvalidated)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct KeepAliveQuery {
    /// 项目 ID（workspace 根目录名）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    /// UserApp 开发卷定位 (可选, 与 projectId 二选一; 见 BuildQuery::app_id)。
    #[serde(default)]
    pub app_id: Option<String>,
    /// 开发服务器进程 PID（start-dev 响应回传的值）
    #[serde(default)]
    #[garde(required)]
    pub pid: Option<u32>,
    /// 开发服务器监听端口
    pub port: u16,
    /// 项目内子路径 (可选；心跳时校验目录仍存在)
    #[serde(default)]
    #[garde(custom(crate::validation_rules::required_not_blank))]
    pub base_path: Option<String>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub isolation_type: Option<String>,
}

// ── project 域 ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[garde(allow_unvalidated)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct GetContentParams {
    /// 项目 ID
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    /// 框架探测命令 (可选；前端框架识别失败时兜底执行的自定义命令)
    pub command: Option<String>,
    /// 代理子路径 (可选；透传给框架探测的环境信息)
    pub proxy_path: Option<String>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[garde(allow_unvalidated)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct GetByVersionParams {
    /// 项目 ID
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    /// 代码版本号（code_version 仓库的版本标签）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub code_version: String,
    /// 代理子路径 (可选；透传给框架探测的环境信息)
    pub proxy_path: Option<String>,
    /// 框架探测命令 (可选；前端框架识别失败时兜底执行的自定义命令)
    #[serde(default)]
    pub command: Option<String>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[garde(allow_unvalidated)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct DeleteParams {
    /// 项目 ID
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    /// 关联 dev server 进程 PID（可选；用于删除前停止开发服务器）
    #[serde(default)]
    pub pid: Option<String>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct CreateProjectBody {
    #[serde(default, deserialize_with = "crate::extract::deserialize_id_string")]
    #[schema(required = true)]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    pub template_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopyProjectBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub source_project_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub target_project_id: String,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub source_tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub source_space_id: Option<String>,
    #[serde(default)]
    pub source_isolation_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub target_tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub target_space_id: Option<String>,
    #[serde(default)]
    pub target_isolation_type: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct BackupVersionBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub code_version: String,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct RollbackBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub code_version: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub rollback_to: String,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct ExportBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub code_version: String,
    #[serde(default)]
    pub export_type: Option<String>,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: Option<serde_json::Value>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct SpecifiedBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub code_version: String,
    pub files: Vec<FileOp>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct AllFilesBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub code_version: String,
    pub files: Vec<FileEntry>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub base_path: Option<String>, // nuwax 接收但未使用
    #[allow(dead_code)]
    #[serde(default)]
    pub pid: Option<String>,
}
