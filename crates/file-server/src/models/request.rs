//! JSON 请求体与 Query 参数（原各 handler 文件内联定义）。
//!
//! 原 `pub(crate)`/私有字段统一放宽为 `pub`（models 是 crate 内公共层）；
//! serde 属性、garde 校验、字段 doc comment 与迁移前逐字节一致。

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

// ── computer 域 ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct UserCidQuery {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    /// 自定义目标目录 (可选；缺省用 user/cid 推导的默认根)
    #[serde(default)]
    #[garde(skip)]
    pub custom_target_dir: Option<String>,
}

/// `get-file-list` 查询参数: 在 `UserCidQuery` 基础上新增 `relativePath` / `recursive`
/// (对齐 TS commit ba08d0c)。缺省 `recursive=true` (原全量递归), 向后兼容。
#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct FileListQuery {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    /// 代理子路径 (可选；网关转发场景透传)
    #[serde(default)]
    #[garde(skip)]
    pub proxy_path: Option<String>,
    /// 自定义目标目录 (可选；缺省用 user/cid 推导的默认根)
    #[serde(default)]
    #[garde(skip)]
    pub custom_target_dir: Option<String>,
    /// 相对工作区根的子目录 (可多级), 空 → 列根目录。
    #[serde(default)]
    #[garde(skip)]
    pub relative_path: Option<String>,
    /// 是否递归扁平列出; 默认 true。显式传 "false" → 仅当前目录一层。
    /// 用 String 接收以对齐 TS `recursive === false || recursive === "false"` 语义。
    #[serde(default)]
    #[garde(skip)]
    pub recursive: Option<String>,
}

/// `resolve-file` 查询参数 (对齐 TS resolveExistingFile)。
#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct ResolveFileQuery {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    /// 代理子路径 (可选；网关转发场景透传)
    #[serde(default)]
    #[garde(skip)]
    pub proxy_path: Option<String>,
    /// 自定义目标目录 (可选；缺省用 user/cid 推导的默认根)
    #[serde(default)]
    #[garde(skip)]
    pub custom_target_dir: Option<String>,
    /// 待解析的文件相对路径 (不补扩展名，逐候选目录查找)
    #[garde(custom(crate::validation_rules::not_blank))]
    pub file_path: String,
}

/// `search-files` 查询参数 (对齐 TS searchFiles)。
/// `limit` / `max_visit` / `timeout_ms` 用 String 接收, 经 garde `positive_int`
/// 校验正整数, 对齐 TS `requirePositiveInt` (由 Java 网关传入, 不设默认值)。
#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilesQuery {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    /// 代理子路径 (可选；网关转发场景透传)
    #[serde(default)]
    #[garde(skip)]
    pub proxy_path: Option<String>,
    /// 自定义目标目录 (可选；缺省用 user/cid 推导的默认根)
    #[serde(default)]
    #[garde(skip)]
    pub custom_target_dir: Option<String>,
    /// 搜索起始子目录 (可多级)，空 → 从工作区根搜起
    #[serde(default)]
    #[garde(skip)]
    pub relative_path: Option<String>,
    /// 关键词（对文件名做大小写敏感包含匹配）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub kw: String,
    /// 返回结果条数上限（正整数，如 "50"）
    #[garde(custom(crate::validation_rules::positive_int))]
    pub limit: String,
    /// 最多访问的目录/文件节点数上限（正整数，防大目录全量遍历）
    #[garde(custom(crate::validation_rules::positive_int))]
    pub max_visit: String,
    /// 搜索超时毫秒数（正整数，超时返回已收集结果）
    #[garde(custom(crate::validation_rules::positive_int))]
    pub timeout_ms: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstallBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    pub programming_language: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildAgentBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    // agentId 同 user_id/c_id: TS 原版 buildAgentPackage 标注 {string|number},Java 后端传 DB bigint(整数)。
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub agent_id: String,
    pub version: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CleanupBuildArtifactsBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    #[serde(default)]
    pub custom_target_dir: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct ExecCommandBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub command: String,
}

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[garde(allow_unvalidated)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct GetLogsQuery {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    /// 读取末尾行数（缺省取最近若干行）
    #[serde(default = "default_tail_lines")]
    pub tail_lines: usize,
}
fn default_tail_lines() -> usize {
    200
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ZipBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    #[serde(default)]
    pub exclude_dirs: Option<Vec<String>>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteWorkspaceBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesUpdateBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    pub files: Vec<FileOp>,
    #[serde(default)]
    pub custom_target_dir: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct GenerateFileBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    /// 文件名，可含相对子路径 (如 "src/foo.txt")；对齐 nuwax normalizeFilePath 会剥离前导 `/`。
    #[garde(custom(crate::validation_rules::not_blank))]
    pub file_name: String,
    /// 文本内容，缺省视为空串。
    #[serde(default)]
    pub content: Option<String>,
    /// 绝对目录覆盖；非空则用之，否则回退默认工作区 (与 upload-file 同语义)。
    #[serde(default)]
    pub custom_target_dir: Option<String>,
}

// ── git 域 ─────────────────────────────────────────────────────────────────────

/// GET 路由公共查询 (workspaceType + project/computer 标识 + 多租户)。
#[derive(Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct GitQuery {
    /// 工作区类型: `project`(项目工作区) / `computer`(Electron 容器根) 二选一
    pub workspace_type: Option<String>,
    /// 项目 ID（workspaceType=project 时必填）
    pub project_id: Option<String>,
    /// 用户 ID（workspaceType=computer 时必填）
    pub user_id: Option<String>,
    /// 容器/实例 ID（workspaceType=computer 时必填）
    pub c_id: Option<String>,
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

/// POST 路由公共 body (写操作基类, 被 FilesBody / CommitBody 等经 serde flatten 复用)。
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitWriteBody {
    pub workspace_type: String,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub project_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub user_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub c_id: Option<String>,
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

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileContentBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    /// nuwax 字段名 `ref` (Rust 关键字, 用 ref_ + serde rename)
    #[serde(rename = "ref", default)]
    pub ref_: Option<String>,
    pub file_path: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommitBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub message: String,
    #[serde(default)]
    #[garde(skip)]
    pub files: Option<Vec<String>>,
    #[serde(default)]
    #[garde(skip)]
    pub author_name: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub author_email: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiffBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub target: String,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResetBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub target: String,
    #[serde(default)]
    #[garde(skip)]
    pub mode: String,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RevertBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub target: String,
    #[serde(default)]
    #[garde(skip)]
    pub message: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub author_name: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub author_email: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BranchCreateBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub branch_name: String,
    #[serde(default)]
    #[garde(skip)]
    pub start_point: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BranchNameBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub branch_name: String,
    /// branch-delete 强制删除未合并分支 (对齐 nuwax deleteBranch force)。
    #[serde(default)]
    #[garde(skip)]
    pub force: Option<bool>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TagCreateBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub tag_name: String,
    #[serde(default)]
    #[garde(skip)]
    pub message: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TagNameBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub tag_name: String,
}

/// git 提交历史查询（无 utoipa 注解：path 参数在 handler 注解里逐项声明，
/// 见 openapi.rs 测试 flattened_git_log_query_is_exposed_as_individual_parameters）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitLogQuery {
    #[serde(flatten)]
    pub base: GitQuery,
    pub max_count: Option<usize>,
    pub skip: Option<usize>,
    /// 指定分支 (对齐 nuwax git.log ref); 默认 HEAD。
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
}
