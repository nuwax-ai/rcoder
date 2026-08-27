//! computer 域请求体与 Query 参数（`{root}/{user_id}/{cId}` Electron 全局根语义）。
//!
//! 字段为 `pub`（models 是 crate 内公共层）；serde 属性、garde 校验与
//! 字段 doc comment 是 wire 契约的一部分，改动须同批核查守卫测试。

use super::code::FileOp;
use garde::Validate;
use serde::Deserialize;

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

/// `/static/{user_id}/{c_id}/*` 的 `?customTargetDir=` 覆盖参数（无 utoipa 派生：
/// path 注解里以单项参数声明，同 GitLogQuery 形态）。
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CustomTargetQuery {
    #[serde(default)]
    pub custom_target_dir: Option<String>,
}
