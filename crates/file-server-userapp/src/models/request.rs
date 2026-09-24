//! workspace / 文件镜像 / dev 生命周期域的请求体、Query 参数与 OpenAPI
//! multipart 占位。
//!
//! serde 属性、garde 校验与字段 doc comment 是 wire 契约的一部分；字段为
//! `pub`（models 是 crate 内公共层）。

use garde::Validate;
use serde::Deserialize;

use file_server::models::BinaryFile;

// ── 构建任务域（userapp.rs）─────────────────────────────────────────────────────

/// `POST /api/v1/userapp/build` 请求体。
#[derive(Debug, Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
pub struct BuildUserAppBody {
    /// Userapp 标识（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）。
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    pub app_id: String,
}

/// detect/confirm 门面同构 body（`{app_id}/{app_stage}` 新形态）：**不含
/// `app_id`**——由路径段提供（handler `Path` 提取）；与 import-project 镜像
/// 接口的 [`ImportProjectBody`]（body 携 app_id 旧契约）并存。
#[derive(Debug, Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
pub struct ProjectChainBody {
    /// workspace 内的子项目目录名（模板 zip 的顶层目录；detect/confirm 的定位粒度）
    #[garde(custom(file_server::validation_rules::not_blank))]
    pub project_dir: String,
}

#[derive(Debug, Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
pub struct ImportProjectBody {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    pub app_id: String,
    /// workspace 内的子项目目录名（模板 zip 的顶层目录；detect/confirm 的定位粒度）
    #[garde(custom(file_server::validation_rules::not_blank))]
    pub project_dir: String,
}

/// SSE 订阅参数（`GET /tasks/{task_id}/logs/stream`）。
///
/// `parameter_in` 必须显式声明：utoipa-axum 自动发现会按 Path extractor 把
/// query 字段误标 path（swagger 对接即错），显式声明优先。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StreamQuery {
    /// 构建链定位 app_id（rcoder 转发层消费：目标开发容器定位与容器不在时的
    /// 短路判定；容器侧校验白名单）
    pub app_id: String,
    /// 从哪个 seq 开始回放（含该 seq；0 = 从头）。仅作兜底——
    /// 请求带 `Last-Event-ID` 头时以头为准（头值 + 1 = 本值语义），query 被忽略。
    #[serde(default)]
    pub from_seq: u64,
}

/// 任务作用域定位参数（`GET /tasks/{task_id}` 与 `POST /tasks/{task_id}/cancel`）。
///
/// `parameter_in` 必须显式声明（同上）。app_id 曾是 rcoder 转发层单方消费的
/// 隐式必填项，本批下沉为容器侧显式校验；user_id 为挂载分区组成段。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserappTaskScopeQuery {
    /// 构建链定位 app_id（rcoder 转发层消费：目标开发容器定位与容器不在时的
    /// 短路判定；容器侧校验白名单）
    pub app_id: String,
}

/// dev 进程列表查询参数（`GET /dev/list`）——按 app_id 过滤单 app 视角。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserappDevListQuery {
    /// 应用 ID（进程表按 `userapp:{app_id}` key 过滤，只返回该 app 的 dev 进程）
    pub app_id: String,
}

/// workspace 框架识别查询参数（`GET /dev/framework-info`）。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserappFrameworkInfoQuery {
    /// 应用 ID（workspace 按 `userapp:{app_id}` 定位，识别其全部服务）
    pub app_id: String,
}

/// static 取包 query（`GET /static/{appId}`）。
///
/// `parameter_in` 必须显式声明：utoipa-axum 从 handler 签名自动发现 Query struct
/// 时按 Path extractor 推断 in（会把 query 字段误标 path——swagger 对接即错），
/// 容器级显式声明优先于自动推断。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StaticQuery {
    /// 可选：按 release_id 精确取包（定位 `builds/workspace-package-{release_id}.zip`）。
    /// 缺省 = 最新产物。release_id 只允许字母数字与连字符（服务端生成的 UUID 形态），
    /// 其余字符一律拒绝（防路径注入）；指定的版本不存在时 404。
    #[serde(default)]
    pub release_id: Option<String>,
}

// ── 开发工作区域（userapp_dev.rs）───────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
pub struct UserappEnsureWorkspaceBody {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
pub struct UserappExecCommandBody {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// shell 命令串（经 shell -c 执行，cwd=workspace）
    pub command: String,
}

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[garde(allow_unvalidated)]
pub struct UserappGetLogsQuery {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[serde(default = "default_tail_lines")]
    /// 返回日志末尾行数；默认 200
    pub tail_lines: usize,
}
fn default_tail_lines() -> usize {
    200
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UserappInstallBody {
    /// 语言：typescript/ts→pnpm install；python/py→pip install
    pub programming_language: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UserappZipBody {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[serde(default)]
    /// 额外排除目录（与内置排除表合并，按任意路径段匹配）
    pub exclude_dirs: Option<Vec<String>>,
}

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserappDownloadQuery {
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[serde(default)]
    #[garde(skip)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
pub struct UserappInitTemplateForm {
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[schema(format = Binary)]
    /// 上传文件（zip 或单文件）
    pub file: String,
    /// 是否 git init（双开关：GIT_ENABLED 且为 true 才执行）
    pub enable_git: Option<bool>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
pub struct UserappPushSkillsForm {
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[schema(format = Binary)]
    /// 上传文件（zip 或单文件）
    pub file: Option<String>,
    /// 技能 zip 的 URL 列表（JSON 数组或单值）
    pub skill_urls: Option<Vec<String>>,
    /// 智能体 ID (开发卷布局下不走 agent-store, 仅审计日志)
    pub agent_id: Option<String>,
}

// ── 文件镜像域（userapp_files.rs）───────────────────────────────────────────────

/// userapp 版 get-file-list 查询参数 (computer FileListQuery 镜像, cId→appId)。
#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserappFileListQuery {
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[serde(default)]
    #[garde(skip)]
    /// 预览 URL 前缀（fileProxyUrl 的 base）；缺省则响应不含 fileProxyUrl
    pub proxy_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 相对 workspace 根的子目录（可多级）；缺省列根目录
    pub relative_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 是否递归展开子目录；缺省 true，显式 "false" 仅当前层
    pub recursive: Option<String>,
    #[serde(default, rename = "type")]
    #[garde(skip)]
    /// 输出类型：all（缺省）/file/dir；directory 是 dir 别名。
    /// 递归模式只筛选原扁平结果中的空目录。
    pub file_type: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 最多输出的条目数，非负整数；0 返回空列表，缺省不限
    pub limit: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserappResolveFileQuery {
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[serde(default)]
    #[garde(skip)]
    /// 预览 URL 前缀（fileProxyUrl 的 base）；缺省则响应不含 fileProxyUrl
    pub proxy_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// workspace 内相对路径的文件（必填非空）
    pub file_path: String,
}

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserappSearchFilesQuery {
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[serde(default)]
    #[garde(skip)]
    /// 预览 URL 前缀（fileProxyUrl 的 base）；缺省则响应不含 fileProxyUrl
    pub proxy_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 相对 workspace 根的子目录（可多级）；缺省列根目录
    pub relative_path: Option<String>,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// 搜索关键字（文件名/相对路径子串，大小写不敏感；必填非空）
    pub kw: String,
    #[garde(custom(file_server::validation_rules::positive_int))]
    /// 命中条数上限（必填正整数）
    pub limit: String,
    #[garde(custom(file_server::validation_rules::positive_int))]
    /// 访问条目数硬上限，含未命中（必填正整数）
    pub max_visit: String,
    #[garde(custom(file_server::validation_rules::positive_int))]
    /// 超时毫秒数（必填正整数）
    pub timeout_ms: String,
}

/// userapp 版 get-file-meta 请求体（computer GetFileMetaBody 镜像, cId→appId）。
/// `file_paths` 非空与批量上限的联合校验在 handler（对齐 computer 域）。
#[derive(Deserialize, Validate, utoipa::ToSchema)]
pub struct UserappFileMetaBody {
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    /// 相对 workspace 根的路径数组（通常为 get-file-list 返回的 name）
    #[garde(skip)]
    pub file_paths: Vec<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 单次批量上限（可选，JSON number；缺省 100，服务端硬顶 1000——仅收
    /// number，非数值 fail-fast 400，与 computer 域同款刻意分歧）
    pub file_meta_max_batch: Option<u64>,
    #[serde(default)]
    #[garde(skip)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UserappFilesUpdateBody {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    /// 增量文件操作列表（与共享 FileOp 同语义，wire 键 snake）
    pub files: Vec<UserappFileOp>,
    #[serde(default)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
}

/// files-update 的单条操作（userapp 域 snake wire；computer 域 FileOp 的
/// camelCase 是 TS 契约不复用——语义同构，经 [`From`] 转入共享核心）。
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct UserappFileOp {
    /// 操作类型：create / delete / rename / modify
    pub operation: String,
    /// 目标文件相对路径（rename 时为新路径）
    pub name: String,
    /// 是否目录（缺省 false）
    #[serde(default)]
    pub is_dir: Option<bool>,
    /// 文本内容（create/modify 时写入；服务端做 URL 解码）
    #[serde(default)]
    pub contents: Option<String>,
    /// rename 的源路径（操作为 rename 时必填）
    #[serde(default)]
    pub rename_from: Option<String>,
}

impl From<UserappFileOp> for file_server::models::FileOp {
    fn from(op: UserappFileOp) -> Self {
        Self {
            operation: op.operation,
            name: op.name,
            is_dir: op.is_dir,
            contents: op.contents,
            rename_from: op.rename_from,
        }
    }
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
pub struct UserappUploadFileForm {
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    /// workspace 内相对路径的文件（必填非空）
    pub file_path: String,
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    /// 上传文件（zip 或单文件）
    pub file: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
pub struct UserappUploadFilesForm {
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    /// 每个文件的目标相对路径（与 files 一一对应，重复字段）
    pub file_paths: Vec<String>,
    /// 上传文件的二进制内容（重复字段，与 file_paths 一一对应）
    pub files: Vec<BinaryFile>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
pub struct UserappGenerateFileBody {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// 文件名，可含相对子路径（如 "src/foo.txt"；自动剥前导 `/`）
    pub file_name: String,
    #[serde(default)]
    /// 文本内容；缺省视为空串
    pub content: Option<String>,
    #[serde(default)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
pub struct UserappImportProjectForm {
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    /// 上传文件（zip 或单文件）
    pub file: String,
}

// ── dev server 生命周期域（userapp_dev_server.rs）───────────────────────────────

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
pub struct DevOpBody {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// Userapp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub app_id: String,
    #[serde(default)]
    #[garde(skip)]
    /// dev server 的 base path（vite --base 等）；缺省 "/"。
    /// **仅 web 域项目（vite dev server）生效**——Userapp workspace
    /// （manifest/app-cli 引擎）不消费：pingap 路由前缀由各服务的
    /// project.manifest.toml `[proxy].path` 决定，传了无效果。
    pub base_path: Option<String>,
    #[serde(default)]
    #[garde(custom(pg_credential))]
    /// PG 数据库凭据（可选，与 prod `StartAppRequest.pg` 同构 wire）：
    /// 给出则注入 dev 编排进程 env 的 `POSTGRES_USER`/`POSTGRES_PASSWORD`
    /// （覆盖容器默认透传值）。这是本次操作捕获的配置，不表示保存已即时改密。
    /// prod 受管账号使用版本化保存/显式生效流程。
    /// start/restart 捕获此输入；recover 只接受原请求一致的凭据；stop 忽略。
    pub pg: Option<shared_types::StartPgCredential>,
}

/// [`DevOpBody::pg`] 的 garde 校验：可选；给出时 username 须过 PG 标识符
/// 白名单、password 非空（与 `/db/reset-password` 同规——受理前 fail-fast，
/// 不触发 workspace IO）。custom 规则收到的是 `&Option<T>`（garde 不自动解包）。
fn pg_credential(pg: &Option<shared_types::StartPgCredential>, _: &()) -> garde::Result {
    let Some(pg) = pg else {
        return Ok(());
    };
    shared_types::pg_utils::validate_pg_identifier(&pg.username).map_err(garde::Error::new)?;
    if pg.password.is_empty() {
        return Err(garde::Error::new("pg.password must not be empty"));
    }
    Ok(())
}
