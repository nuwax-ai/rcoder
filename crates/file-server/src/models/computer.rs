//! computer 域请求体与 Query 参数（`{root}/{user_id}/{cId}` Electron 全局根语义）。
//!
//! 字段为 `pub`（models 是 crate 内公共层）；serde 属性、garde 校验与
//! 字段 doc comment 是 wire 契约的一部分，改动须同批核查守卫测试。
//!
//! 项目绑定目录 `workspacePath`（可选，对齐 TS nuwax-file-server 1.4.5）：全部
//! 契约统一携带；值为一跨平台绝对路径，非空时优先于默认定位（与
//! `x-workspace-path` header 同语义，header 优先），合法性由收口
//! `computer_root_for_request` fail-fast 校验。

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
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    #[garde(skip)]
    pub workspace_path: Option<String>,
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
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    #[garde(skip)]
    pub workspace_path: Option<String>,
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
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    #[garde(skip)]
    pub workspace_path: Option<String>,
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
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    #[garde(skip)]
    pub workspace_path: Option<String>,
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
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    /// 语言：typescript/ts → pnpm install；python/py → pip install
    pub programming_language: String,
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    pub workspace_path: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildAgentBody {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    // agentId 同 user_id/c_id: TS 原版 buildAgentPackage 标注 {string|number},Java 后端传 DB bigint(整数)。
    /// 智能体 ID（Java 后端传 DB bigint，此处字符串化）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub agent_id: String,
    /// 安装包版本号
    pub version: String,
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    pub workspace_path: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CleanupBuildArtifactsBody {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    /// 自定义目标目录（可选；缺省用 user/cid 推导的默认根）
    #[serde(default)]
    pub custom_target_dir: Option<String>,
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    pub workspace_path: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct ExecCommandBody {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub c_id: String,
    /// shell 命令串（经 shell -c 执行，cwd=workspace）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub command: String,
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    #[garde(skip)]
    pub workspace_path: Option<String>,
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
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时日志目录为 `{绑定}/.logs`，对齐 TS 1.4.5）
    #[serde(default)]
    #[garde(skip)]
    pub workspace_path: Option<String>,
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
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    /// 额外排除目录（与内置排除表合并，按任意路径段匹配）
    #[serde(default)]
    pub exclude_dirs: Option<Vec<String>>,
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    pub workspace_path: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteWorkspaceBody {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    /// 用户维度工作目录（可选；非空时直接定位该目录删除，不先建后删，对齐 TS 1.4.5）
    #[serde(default)]
    pub workspace_path: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesUpdateBody {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub c_id: String,
    /// 增量文件操作列表（modify 用字节比较）
    pub files: Vec<FileOp>,
    /// 自定义目标目录（可选；缺省用 user/cid 推导的默认根）
    #[serde(default)]
    pub custom_target_dir: Option<String>,
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    pub workspace_path: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub struct GenerateFileBody {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
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
    /// 用户维度工作目录（可选；跨平台绝对路径，非空时优先于默认定位，对齐 TS 1.4.5）
    #[serde(default)]
    #[garde(skip)]
    pub workspace_path: Option<String>,
}

/// `/static/{user_id}/{c_id}/*` 的 `?customTargetDir=` 覆盖参数（无 utoipa 派生：
/// path 注解里以单项参数声明，同 GitLogQuery 形态）。
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CustomTargetQuery {
    #[serde(default)]
    pub custom_target_dir: Option<String>,
    /// 用户维度工作目录（可选；非空时静态文件从该目录解析，对齐 TS 1.4.5 server.js）
    #[serde(default)]
    pub workspace_path: Option<String>,
}

/// `fs/children` 查询参数（目录浏览，不锚定工作区、不带会话上下文；对齐 TS
/// `listFsChildren`——绝对路径校验用宿主语义，等价 TS `path.isAbsolute`）。
#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct FsChildrenQuery {
    /// 待浏览的绝对目录路径（宿主语义绝对路径：POSIX `/a/b`、Windows `C:/a/b`）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub path: String,
}
