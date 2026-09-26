//! 响应载荷 DTO。
//!
//! `SkillFailure` / `KilledPid` / `LogLine` / `ReadDevLogResult` 原定义在
//! service 层但被 wire 契约内嵌（既是运行时数据又是响应形状）——归入 models
//! 后 service 继续从这里引用（对齐 app_manager service 依赖 models 的形态）。

use serde::Serialize;
use utoipa::ToSchema;

// ── System ──────────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    /// 恒为 "ok"（探活判定依据）
    pub status: String,
    /// 当前 epoch 毫秒
    pub timestamp: i64,
    /// 进程运行秒数
    pub uptime: u64,
    /// 服务版本号
    pub version: String,
    /// 运行平台标识（os）
    pub platform: String,
    /// 运行时标识（Rust 版，对齐 nuwax nodeVersion 字段位）
    pub node_version: String,
    /// 进程 PID
    pub pid: u32,
    /// 内存占用明细（MB）
    pub memory: MemoryUsage,
    /// 运行环境标识（NODE_ENV，缺省 unknown）
    pub env: String,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryUsage {
    /// 常驻内存（MB）
    pub rss: f64,
    /// 堆已用（MB；Rust 无 GC 堆，恒 0）
    pub heap_used: f64,
    /// 堆总量（MB；恒 0）
    pub heap_total: f64,
    /// 外部内存（MB；恒 0）
    pub external: f64,
}

/// `/api/version` 响应 (对齐 TS `{ success: true, version }`)。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct VersionResponse {
    /// 恒为 true
    pub success: bool,
    /// 服务版本号
    pub version: String,
}

// ── computer 工作区 ─────────────────────────────────────────────────────────────

/// 单个 skill URL 推送失败 (best-effort 语义下收集, 透传给调用方)。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct SkillFailure {
    /// 推送失败的 skill URL
    pub url: String,
    /// 失败原因
    pub error: String,
}

/// create-workspace 响应 (对齐 nuwax createWorkspace 响应字段)。
/// workspaceRoot = COMPUTER_WORKSPACE_DIR; updatedSkills/failedSkills 空时不输出。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceResponse {
    /// 恒为 true；失败走错误响应
    pub success: bool,
    /// 结果文案
    pub message: String,
    /// 工作区根路径 (COMPUTER_WORKSPACE_DIR)
    pub workspace_root: String,
    /// 已推送的技能目录名列表（空时不输出）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub updated_skills: Vec<String>,
    /// best-effort 透传: 推送失败的 skill URL 明细 (空则不输出)。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed_skills: Vec<SkillFailure>,
    /// Agent Store v2 字段；legacy workspace 模式不输出。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_store_path: Option<String>,
    /// 因重名等原因跳过的技能目录名（无跳过时不输出）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_skills: Option<Vec<String>>,
    /// 是否跳过了 store 清单更新（未跳过时不输出）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_store_update: Option<bool>,
}

// ── dev server（file-server 本体与 file-server-userapp 跨 crate 共享）──────────

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct KilledPid {
    /// 被杀进程 PID
    pub pid: u32,
    /// 是否杀灭成功
    pub killed: bool,
}

/// 一行日志 (对齐 nuwax getDevLog 响应)。
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct LogLine {
    /// 行号（1 起）
    pub line: usize,
    /// 日志行内容
    pub content: String,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ReadDevLogResult {
    /// 日志行列表（含行号，snake_case wire）
    pub logs: Vec<LogLine>,
    /// 该日志文件总行数（分页导航：start_index 超过它表示读完）
    pub total_lines: usize,
    /// 本批起始行号（1-based）
    pub start_index: usize,
    /// 实际读取的日志文件名（按日期滚动的当前文件）
    pub log_file_name: String,
}

// ── build / dev server 生命周期（TS 对齐域：裸 {success, message, ...} 信封）──

/// start-dev / restart-dev 响应 (对齐 nuwax: {success, message, projectId, pid, port})。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DevStarted {
    /// 操作是否成功
    pub success: bool,
    /// 启动结果消息
    pub message: String,
    /// 项目 ID
    pub project_id: String,
    /// dev server 主进程 PID（keep-alive 心跳回传它）
    pub pid: u32,
    /// dev server 监听端口
    pub port: u16,
}

/// stop-dev 响应 (pid 恒 null: Option 不加 skip_serializing_if → 序列化为 null, 对齐现 json!)。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DevStopped {
    /// 操作是否成功
    pub success: bool,
    /// 停止结果消息
    pub message: String,
    /// 项目 ID
    pub project_id: String,
    /// 恒 null（按 app 定位进程组，无需 pid）
    pub pid: Option<u32>,
    /// 被杀进程 PID 明细（killed 标记是否杀灭成功）
    pub killed_pids: Vec<KilledPid>,
}

/// list-dev 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DevList {
    /// 操作是否成功
    pub success: bool,
    /// 在跑的 dev server 进程列表
    pub list: Vec<DevProcess>,
}

/// keep-alive 响应 (action 仅重启分支有 → None 时省略, 匹配现 json! 条件追加行为)。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct KeepAlive {
    /// 操作是否成功
    pub success: bool,
    /// 项目 ID
    pub project_id: String,
    /// 主进程 PID
    pub pid: u32,
    /// 监听端口
    pub port: u16,
    /// 心跳结果消息
    pub message: String,
    /// 心跳结果动作（"restarted" = 探活失败已重启；存活时省略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// 降级原因（宿主不可达/实例未知/端口不符——多副本协调新增；存活/重启时省略）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// port-pool-status 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortPool {
    /// 操作是否成功
    pub success: bool,
    /// 结果消息
    pub message: String,
    /// 端口池范围（如 "4000-55000"，保留区已剔除）
    pub port_range: String,
    /// 已分配端口数
    pub total_allocated: usize,
    /// projectId → port 分配明细
    pub allocations: Vec<PortAllocation>,
}

/// get-dev-log 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DevLog {
    /// 操作是否成功
    pub success: bool,
    /// 结果消息
    pub message: String,
    /// 日志行列表（含行号）
    pub logs: Vec<LogLine>,
    /// 该日志文件总行数（分页导航）
    pub total_lines: usize,
    /// 本批起始行号（1-based）
    pub start_index: usize,
    /// 实际读取的日志文件名（按日期滚动的当前文件）
    pub log_file_name: String,
    /// 是否命中服务端日志缓存（未命中才读盘）
    pub cache_hit: bool,
    /// 文件超过缓存上限时置 true（此时为直接读盘的部分内容）
    pub file_too_large: bool,
}

/// parse-build-error / clear-all-log-cache 共用 {success, message} 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Simple {
    /// 操作是否成功
    pub success: bool,
    /// 结果消息
    pub message: String,
}

/// get-log-cache-stats 响应 (stats 内含 SCREAMING_SNAKE 键 → 逐字段 rename)。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogCacheStats {
    /// 操作是否成功
    pub success: bool,
    /// 结果消息
    pub message: String,
    /// 日志缓存配置与运行时统计
    pub stats: LogCacheStatsData,
}

#[derive(Serialize, ToSchema)]
pub struct LogCacheStatsData {
    /// 日志缓存功能是否启用
    pub enabled: bool,
    #[serde(rename = "cacheSize")]
    /// 当前缓存占用字节数
    pub cache_size: u64,
    #[serde(rename = "maxCacheEntries")]
    /// 最大缓存条目数
    pub max_cache_entries: u64,
    #[serde(rename = "cacheDuration")]
    /// 缓存条目存活秒数
    pub cache_duration: u64,
    #[serde(rename = "maxFileSizeMB")]
    /// 单文件缓存上限（MB，展示串）
    pub max_file_size_mb: String,
    #[serde(rename = "totalCacheSizeMB")]
    /// 缓存总占用（MB，展示串）
    pub total_cache_size_mb: String,
    #[serde(rename = "NODE_ENV")]
    /// 运行环境标识（对齐 nuwax 透传 NODE_ENV）
    pub node_env: String,
    #[serde(rename = "LOG_CACHE_ENABLED")]
    /// 日志缓存开关（对齐 nuwax 配置键名）
    pub log_cache_enabled: bool,
}

/// build 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildDone {
    /// 操作是否成功
    pub success: bool,
    /// 构建结果消息
    pub message: String,
    /// 项目 ID
    pub project_id: String,
}

/// 运行中的 dev server 记录（内存状态 + list-dev wire 双面；log_dir/temp_log_name
/// 不上 wire）。DevServerManager 内存状态直接持有本类型。
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DevProcess {
    /// 主进程 PID
    pub pid: u32,
    /// 监听端口
    pub port: u16,
    /// 项目 ID（workspace 根目录名）
    pub project_id: String,
    /// 启动时间（Unix 毫秒）
    pub started_at: i64,
    /// 预览实例 ID（协调票据模式登记；legacy 启动为 None，不上 wire）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// 探活 base path（协调票据模式登记；内部状态不上 wire）
    #[serde(skip)]
    pub base_path: Option<String>,
    #[serde(skip)]
    pub log_dir: std::path::PathBuf,
    #[serde(skip)]
    pub temp_log_name: String,
    /// 外部 owner（P3-02）：workspace 由非本管理器启动的 app-cli serve
    /// owner 托管——停止/查询经运行 API 路由，不走进程信号。None = 本
    /// 管理器自有子进程（legacy 语义不变）。
    #[serde(skip)]
    pub external_owner: Option<ExternalOwner>,
}

/// 外部 owner 连接信息（凭据仅存内存，不上 wire/日志）。
#[derive(Debug, Clone)]
pub struct ExternalOwner {
    /// 管理 API 地址（127.0.0.1:3010）
    pub address: String,
    /// X-Deploy-Token（状态根 token 文件读取）
    pub token: String,
    /// owner 运行实例 ID（操作提交的期望实例）
    pub runtime_instance_id: String,
}

/// 端口池分配明细（port-pool-status 内嵌；PortPoolStatus 快照持有同型列表）。
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortAllocation {
    /// 占用方项目 ID
    pub project_id: String,
    /// 分配到的端口
    pub port: u16,
}

// ── 文件系统目录浏览 (/fs/roots, /fs/children, /fs/mkdir, /fs/rename, 对齐 TS 1.5.1) ──

/// 目录浏览条目（目录与文件；是否可选由前端按 `isDir` 判断）。
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    /// 条目名（不含路径）
    pub name: String,
    /// 归一化显示路径（分隔符统一 `/`，UNC 前导保留）
    pub path: String,
    /// 是否目录（符号链接按目标类型判定）
    pub is_dir: bool,
    /// 是否符号链接（前端标注用；悬空链接按文件展示）
    pub is_symlink: bool,
}

/// 文件系统根条目（win32 盘符 / 其余平台 `/`）。
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsRootEntry {
    /// 显示名（如 `C:/` 或 `/`）
    pub name: String,
    /// 根路径
    pub path: String,
    /// 恒为 true（根必为目录）
    pub is_dir: bool,
}

/// `GET /fs/roots` 响应：浏览起点（根列表 + home 快捷入口）。
#[derive(Debug, serde::Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsRootsResponse {
    /// 恒 true
    pub success: bool,
    /// 可进入的根目录列表
    pub roots: Vec<FsRootEntry>,
    /// 用户 home（前端快捷入口；理论上缺省为 null）
    pub home: Option<String>,
}

/// `GET /fs/children` 响应：某绝对目录下一层子项。
#[derive(Debug, serde::Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsChildrenResponse {
    /// 恒 true
    pub success: bool,
    /// 归一化后的请求目录路径
    pub path: String,
    /// 一层子项（目录在前、名称自然排序）
    pub entries: Vec<FsEntry>,
}

/// `POST /fs/mkdir` / `POST /fs/rename` 响应：新建/重命名后的目录条目
/// （对齐 TS 1.5.1 目录选择弹窗写操作）。
#[derive(Debug, serde::Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsMutationResponse {
    /// 恒 true
    pub success: bool,
    /// 条目名（`dirName`/`newName` trim 后的值）
    pub name: String,
    /// 归一化后的完整路径（分隔符统一 `/`）
    pub path: String,
    /// 归一化后的父目录路径
    pub parent_path: String,
    /// 恒 true（目录选择弹窗当前仅目录操作）
    pub is_dir: bool,
    /// 恒 false
    pub is_symlink: bool,
}

/// 项目/工作区文件树条目（get-project-content 等直接序列化的 wire 契约）。
/// 可选键缺省时不序列化（对齐 TS 展开写法）。命名 FileTreeEntry 以区别于
/// [`crate::models::code`] 的全量更新请求项 FileEntry。
#[derive(Serialize, Debug, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileTreeEntry {
    /// 文件/目录名（不含路径）
    pub name: String,
    /// 是否目录
    pub is_dir: bool,
    /// 二进制内容标记（仅文件内容读取场景携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    /// 超限标记（内容超过上限时携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_exceeded: Option<bool>,
    /// 文本内容（仅内容读取场景携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contents: Option<String>,
    /// 预览代理 URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_proxy_url: Option<String>,
    /// 是否符号链接
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_link: Option<bool>,
}

// ── computer 域响应载荷（wire 契约，对齐 nuwax；字段名/存在性以 A/B 实测为准）──

/// computer 文件列表/搜索条目。TS 契约是多态形状：目录条目只有
/// `name`/`isDir` 两键，文件条目另有恒存在的 `fileProxyUrl`（无代理时为
/// null）与 `isLink`。
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum ComputerFileEntry {
    /// 目录条目（仅两键）
    #[serde(rename_all = "camelCase")]
    Directory {
        /// 文件/目录名（不含路径）
        name: String,
        /// 是否目录
        is_dir: bool,
    },
    /// 文件条目
    #[serde(rename_all = "camelCase")]
    File {
        /// 文件/目录名（不含路径）
        name: String,
        /// 是否目录（文件条目恒 false）
        is_dir: bool,
        /// 预览代理 URL；未提供 proxyPath 时为 null
        file_proxy_url: Option<String>,
        /// 是否符号链接（缺省按非链接序列化）
        is_link: bool,
    },
}

impl From<crate::service::tree::FileEntry> for ComputerFileEntry {
    fn from(file: crate::service::tree::FileEntry) -> Self {
        if file.is_dir {
            Self::Directory {
                name: file.name,
                is_dir: true,
            }
        } else {
            Self::File {
                name: file.name,
                is_dir: false,
                file_proxy_url: file.file_proxy_url,
                is_link: file.is_link.unwrap_or(false),
            }
        }
    }
}

/// get-file-list 响应。`limit` 未指定时序列化为 null（键恒存在）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileListResult {
    /// 恒为 true；失败走错误响应
    pub success: bool,
    /// 条目列表（目录不存在时为空数组）
    pub files: Vec<ComputerFileEntry>,
    /// 是否递归列出
    pub recursive: bool,
    /// 生效的过滤类型（all/file/dir）
    #[serde(rename = "type")]
    pub file_type: String,
    /// 生效的条数上限（未限制时为 null）
    pub limit: Option<usize>,
}

/// resolve-file 响应。TS 契约按存在性多态：未命中只有 `success`/`exists`，
/// 命中才有 `name` 与 `fileProxyUrl`。
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum ResolveFileResult {
    /// 未命中（不存在 / 根目录缺失）
    #[serde(rename_all = "camelCase")]
    Missing {
        /// 恒为 true
        success: bool,
        /// 文件是否存在
        exists: bool,
    },
    /// 命中
    #[serde(rename_all = "camelCase")]
    Found {
        /// 恒为 true
        success: bool,
        /// 文件是否存在
        exists: bool,
        /// 命中文件名
        name: String,
        /// 预览代理 URL（无 proxyPath 时为 null）
        file_proxy_url: Option<String>,
    },
}

/// search-files 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilesResult {
    /// 恒为 true；失败走错误响应
    pub success: bool,
    /// 命中条目（有界实时搜索）
    pub files: Vec<ComputerFileEntry>,
    /// 是否因 limit/预算截断
    pub truncated: bool,
    /// 实际遍历的目录数
    pub visited: usize,
}

/// get-file-meta 单条元数据。基础键恒存在（无值为 null）；`error` 仅失败
/// 条目携带（成功条目无该键，对齐 TS 展开写法）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileMetaEntryResult {
    /// 请求的文件路径（原样回显）
    pub path: String,
    /// 是否目录
    pub is_dir: Option<bool>,
    /// 是否符号链接
    pub is_link: Option<bool>,
    /// 文件字节数（目录为 null）
    pub size: Option<u64>,
    /// 修改时间（epoch 毫秒，浮点）
    pub mtime_ms: Option<f64>,
    /// 扩展名（不含点；无扩展为 null）
    pub extension: Option<String>,
    /// MIME 类型
    pub mime_type: Option<String>,
    /// 符号链接目标（非链接为 null）
    pub link_target: Option<String>,
    /// 目录直接子项数（文件为 null）
    pub child_count: Option<u64>,
    /// 单条失败原因（仅失败条目存在该键）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// get-file-meta 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileMetaResult {
    /// 恒为 true
    pub success: bool,
    /// 与请求同序的元数据条目
    pub metas: Vec<FileMetaEntryResult>,
}

/// computer files-update 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesUpdateResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 回显请求的用户 ID
    pub user_id: String,
    /// 回显请求的实例 ID
    pub c_id: String,
    /// 本次生效的文件操作数
    pub files_count: usize,
}

/// generate-file 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenerateFileResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 生成文件的相对路径
    pub file_name: String,
    /// 生成内容字节数
    pub file_size: u64,
}

/// upload-file 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFileResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 上传内容字节数
    pub file_size: u64,
}

/// upload-files 单条结果。成功条目携带 `message`，失败条目携带 `error`。
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum UploadResultItem {
    /// 上传成功
    #[serde(rename_all = "camelCase")]
    Ok {
        /// 该条是否成功
        success: bool,
        /// 目标相对路径
        file_path: String,
        /// 原始文件名（表单缺失时为 null）
        originalname: Option<String>,
        /// 固定文案
        message: String,
        /// 内容字节数
        file_size: u64,
    },
    /// 上传失败
    #[serde(rename_all = "camelCase")]
    Err {
        /// 该条是否成功
        success: bool,
        /// 目标相对路径
        file_path: String,
        /// 原始文件名（表单缺失时为 null）
        originalname: Option<String>,
        /// 失败原因
        error: String,
    },
}

/// upload-files 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFilesResult {
    /// 批量受理恒为 true（单条失败见 results）
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 总条数
    pub total_count: usize,
    /// 成功条数
    pub success_count: usize,
    /// 失败条数
    pub fail_count: usize,
    /// 与上传同序的逐条结果
    pub results: Vec<UploadResultItem>,
}

/// execute-command 响应。外层恒 success=true，命令结果由 exitCode 表示。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteCommandResult {
    /// 恒为 true；失败走错误响应
    pub success: bool,
    /// 标准输出
    pub stdout: String,
    /// 标准错误
    pub stderr: String,
    /// 命令退出码
    pub exit_code: i64,
}

/// computer get-logs 响应。`logFileName` 无日志文件时为 null（键恒存在）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerLogsResult {
    /// 恒为 true
    pub success: bool,
    /// 结果说明（空日志时为原因文案）
    pub message: String,
    /// 日志行（行号从 1 起）
    pub logs: Vec<LogLine>,
    /// 日志总行数
    pub total_lines: usize,
    /// 本页起始行号
    pub start_index: usize,
    /// 日志文件名（无日志文件时为 null）
    pub log_file_name: Option<String>,
}

/// install-project 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstallProjectResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 安装发生的项目目录（绝对路径）
    pub project_dir: String,
    /// 识别的编程语言
    pub programming_language: String,
}

/// build-agent-package 单个产物。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentArtifact {
    /// 产物相对工作区路径
    pub path: String,
    /// 产物文件名
    pub file_name: String,
    /// 目标平台标识（如 linux-x64）
    pub platform: String,
}

/// build-agent-package 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildAgentPackageResult {
    /// 恒为 true
    pub success: bool,
    /// 打包产物列表
    pub artifacts: Vec<AgentArtifact>,
}

/// cleanup-build-artifacts 响应（无 message 字段）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CleanupBuildArtifactsResult {
    /// 恒为 true
    pub success: bool,
    /// 是否实际清理了产物目录
    pub cleaned: bool,
}

/// import-project 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 回显请求的用户 ID
    pub user_id: String,
    /// 回显请求的实例 ID
    pub c_id: String,
    /// 导入落地的目标目录
    pub target_dir: String,
}

/// delete-workspace 响应（不存在视为已删除）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteWorkspaceResult {
    /// 恒为 true
    pub success: bool,
    /// 是否删除（目录不存在时仍为 true）
    pub deleted: bool,
}

/// init-project-template 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitProjectTemplateResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 初始化的工作区根路径
    pub workspace_root: String,
}

/// push-skills 响应。`agentStorePath` 仅实体存储路径存在时携带。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushSkillsResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（按更新技能数/是否实体存储生成）
    pub message: String,
    /// 工作区根路径
    pub workspace_root: String,
    /// 已推送的技能目录名列表
    pub updated_skills: Vec<String>,
    /// agent 实体存储路径（未走实体存储时无该键）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_store_path: Option<String>,
}

// ── git 域响应载荷（wire 契约；CommitInfo 蛇形字段名是既有 wire，不改 camelCase）──

/// git log 单条提交（字段名 `author_name`/`author_email` 为既有 wire 蛇形名）。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CommitInfo {
    /// 提交哈希（完整 SHA-1）
    pub hash: String,
    /// 提交时间（RFC 3339 / ISO 字符串）
    pub date: String,
    /// 提交消息
    pub message: String,
    /// 作者名
    pub author_name: String,
    /// 作者邮箱
    pub author_email: String,
}

/// git status 响应。ahead/behind/tracking 为占位字段（gix 未实现远端追踪，
/// 固定 0/0/null，对齐 TS wire）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusResult {
    /// 恒为 true；失败走错误响应
    pub success: bool,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 当前分支名（ unborn 仓库为 null）
    pub current: Option<String>,
    /// 已暂存路径
    pub staged: Vec<String>,
    /// 工作区已修改路径
    pub modified: Vec<String>,
    /// 新增（已暂存）路径
    pub created: Vec<String>,
    /// 删除路径
    pub deleted: Vec<String>,
    /// 未跟踪路径（叶子路径）
    pub untracked: Vec<String>,
    /// 冲突路径（merge 未解决）
    pub conflicted: Vec<String>,
    /// 领先远端提交数（占位 0）
    pub ahead: usize,
    /// 落后远端提交数（占位 0）
    pub behind: usize,
    /// 上游追踪分支（占位 null）
    pub tracking: Option<String>,
}

/// branches 响应中的单分支条目。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitBranchEntry {
    /// 分支名
    pub name: String,
    /// 是否当前检出分支
    pub current: bool,
}

/// git branches 响应（branches 为按名索引的对象，对齐 nuwax）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitBranchesResult {
    /// 恒为 true
    pub success: bool,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 分支表（键为分支名）
    pub branches: std::collections::BTreeMap<String, GitBranchEntry>,
    /// 当前分支名
    pub current: Option<String>,
}

/// git tags 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitTagsResult {
    /// 恒为 true
    pub success: bool,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 标签列表（排序）
    pub tags: Vec<String>,
    /// 最新标签（无标签为 null）
    pub latest: Option<String>,
}

/// git log 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitLogResult {
    /// 恒为 true；空仓库/不存在 ref 返回空列表
    pub success: bool,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 提交列表（新到旧）
    pub commits: Vec<CommitInfo>,
    /// 返回条数
    pub total: usize,
}

/// git file-content 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitFileContentResult {
    /// 恒为 true
    pub success: bool,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 读取的文件相对路径
    pub file_path: String,
    /// 读取的 ref（worktree/staged/空 仍回显原值）
    #[serde(rename = "ref")]
    pub ref_spec: String,
    /// 文件内容
    pub content: String,
}

/// git init 响应（幂等）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitInitResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（区分已存在/新初始化）
    pub message: String,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 是否原本已是仓库
    pub already_exists: bool,
}

/// git add 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitAddResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 本次操作日志 ID
    pub log_id: String,
}

/// git commit 响应。两态：正常提交携带 commit/summary；无可提交携带
/// `nothingToCommit: true`。
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum GitCommitResult {
    /// 提交成功
    Committed {
        /// 恒为 true
        success: bool,
        /// 固定文案
        message: String,
        /// 本次操作日志 ID
        log_id: String,
        /// 新提交哈希
        commit: String,
        /// 提交摘要
        summary: GitCommitSummary,
    },
    /// 暂存区为空
    NothingToCommit {
        /// 恒为 true
        success: bool,
        /// 固定文案
        message: String,
        /// 本次操作日志 ID
        log_id: String,
        /// 无可提交标记
        nothing_to_commit: bool,
    },
}

/// commit 摘要。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitSummary {
    /// 变更文件数
    pub changes: usize,
}

/// unstage 的 files 回显：全量为字符串 "all"，否则为文件路径数组。
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum GitUnstageFiles {
    /// 全量取消暂存时的字面量
    All(String),
    /// 指定文件路径
    Files(Vec<String>),
}

/// git unstage 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitUnstageResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（区分全量/指定）
    pub message: String,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 回显的文件集合（"all" 或路径数组）
    pub files: GitUnstageFiles,
}

/// diff summary 单文件统计。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffFileStat {
    /// 文件路径
    pub file: String,
    /// 变更总数（新增+删除）
    pub changes: usize,
    /// 新增行数
    pub insertions: usize,
    /// 删除行数
    pub deletions: usize,
    /// 是否二进制文件
    pub binary: bool,
}

/// diff 汇总。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffSummary {
    /// 逐文件统计
    pub files: Vec<GitDiffFileStat>,
    /// 总新增行数
    pub insertions: usize,
    /// 总删除行数
    pub deletions: usize,
}

/// git diff 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffResult {
    /// 恒为 true
    pub success: bool,
    /// 本次操作日志 ID
    pub log_id: String,
    /// diff 来源（worktree/staged/commit）
    pub source: String,
    /// unified diff 文本
    pub diff: String,
    /// 变更汇总
    pub summary: GitDiffSummary,
}

/// git reset 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitResetResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（含 mode 与 target）
    pub message: String,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 重置目标（ref/哈希）
    pub target: String,
    /// 重置模式（soft/mixed/hard）
    pub mode: String,
    /// 重置前 HEAD（无 HEAD 为 null）
    pub previous_head: Option<String>,
}

/// git checkout 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitCheckoutResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（含 target）
    pub message: String,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 检出目标
    pub target: String,
}

/// git revert 响应。两态：产生新提交（commit/previousHead）或目标态与当前一致
/// （`nothingToCommit: true`）。
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum GitRevertResult {
    /// 产生回退提交
    Reverted {
        /// 恒为 true
        success: bool,
        /// 固定文案
        message: String,
        /// 本次操作日志 ID
        log_id: String,
        /// 新提交哈希
        commit: String,
        /// 回退目标
        target: String,
        /// 回退前 HEAD
        previous_head: String,
    },
    /// 目标态与当前一致，未产生提交
    NothingToRevert {
        /// 恒为 true
        success: bool,
        /// 固定文案
        message: String,
        /// 本次操作日志 ID
        log_id: String,
        /// 无可提交标记
        nothing_to_commit: bool,
        /// 回退目标
        target: String,
    },
}

/// branch-create/delete/switch 响应（三接口共用形状）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitBranchOpResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 分支名
    pub branch_name: String,
}

/// tag-create/delete 响应（两接口共用形状）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitTagOpResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 本次操作日志 ID
    pub log_id: String,
    /// 标签名
    pub tag_name: String,
}

// ── project 域响应载荷 ──

/// get-project-content 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectContentResult {
    /// 恒为 true；失败走错误响应
    pub success: bool,
    /// 项目文件树（扁平条目，含 fileProxyUrl）
    pub files: Vec<FileTreeEntry>,
    /// 前端框架标识
    pub frontend_framework: String,
    /// 开发框架标识
    pub dev_framework: String,
}

/// get-project-content-by-version 响应（版本快照文件树）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectVersionContentResult {
    /// 恒为 true
    pub success: bool,
    /// 版本快照文件树
    pub files: Vec<FileTreeEntry>,
}

/// create-project 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateProjectResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（含项目 ID）
    pub message: String,
    /// 项目根路径
    pub project_path: String,
}

/// copy-project 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopyProjectResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（含源/目标 ID）
    pub message: String,
    /// 源项目 ID
    pub source_project_id: String,
    /// 目标项目 ID
    pub target_project_id: String,
    /// 目标项目根路径
    pub target_project_path: String,
}

/// delete-project 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteProjectResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（含失败目录数提示）
    pub message: String,
    /// 项目 ID
    pub project_id: String,
    /// 已删除目录列表
    pub deleted_directories: Vec<String>,
    /// 删除失败目录列表（空为全成功；条目含失败原因）
    pub failed_directories: Vec<FailedDirEntry>,
}

/// 删除失败目录条目（既有 wire 形状：路径 + 失败原因）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailedDirEntry {
    /// 失败的目录路径
    pub path: String,
    /// 失败原因
    pub error: String,
}

/// specified-files-update 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpecifiedFilesUpdateResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 项目 ID
    pub project_id: String,
    /// 生效文件数
    pub files_count: usize,
}

/// all-files-update 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AllFilesUpdateResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 项目 ID
    pub project_id: String,
    /// 是否重启了 dev server（当前恒 false，对齐 TS wire）
    pub restarted: bool,
}

/// upload-single-file 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadSingleResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案（无需重启）
    pub message: String,
    /// 项目 ID
    pub project_id: String,
    /// 是否重启了 dev server（恒 false）
    pub restarted: bool,
}

/// upload-batch-files 单文件条目（既有 wire 字段名 filePath/size）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectBatchFile {
    /// 写入的相对路径
    pub file_path: String,
    /// 内容字节数
    pub size: u64,
}

/// upload-batch-files 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadBatchResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（含数量）
    pub message: String,
    /// 项目 ID
    pub project_id: String,
    /// 写入文件数
    pub file_count: usize,
    /// 逐文件路径与大小
    pub files: Vec<ProjectBatchFile>,
    /// 是否重启了 dev server（恒 false）
    pub restarted: bool,
}

/// upload-attachment-file 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadAttachmentResult {
    /// 恒为 true
    pub success: bool,
    /// 落地文件名
    pub file_name: String,
    /// 附件相对路径
    pub relative_path: String,
}

/// upload-project 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadProjectResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（含项目 ID）
    pub message: String,
    /// 项目 ID
    pub project_id: String,
    /// 生效 codeVersion
    pub code_version: String,
}

/// project push-skills 响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPushSkillsResult {
    /// 恒为 true
    pub success: bool,
    /// 结果文案（按更新技能数生成）
    pub message: String,
    /// 项目根路径
    pub project_path: String,
    /// 已推送技能目录名列表
    pub updated_skills: Vec<String>,
}

/// backup-current-version（git 未启用分支）响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupVersionResult {
    /// 恒为 true
    pub success: bool,
    /// 项目 ID
    pub project_id: String,
    /// 备份 zip 路径
    pub zip_path: String,
}

/// rollback-version（git 未启用分支）响应。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RollbackVersionResult {
    /// 恒为 true
    pub success: bool,
    /// 固定文案
    pub message: String,
    /// 回滚产生的新版本号
    pub new_version: u64,
    /// 回滚到的目标版本
    pub rollback_to: u64,
}

impl DeprecatedResult {
    /// 构造废弃引导响应（HTTP 200 + `{success:false, deprecated:true, message}`）。
    pub fn new(message: &str) -> Self {
        Self {
            success: false,
            deprecated: true,
            message: message.to_string(),
        }
    }
}

/// backup-current-version 响应。git 启用时返回废弃引导，否则返回备份结果。
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum BackupVersionResponse {
    /// 备份成功
    Ok(BackupVersionResult),
    /// 已废弃（引导改用 /api/git/*）
    Deprecated(DeprecatedResult),
}

/// rollback-version 响应。git 启用时返回废弃引导，否则返回回滚结果。
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum RollbackVersionResponse {
    /// 回滚成功
    Ok(RollbackVersionResult),
    /// 已废弃（引导改用 /api/git/*）
    Deprecated(DeprecatedResult),
}

/// 已废弃接口响应（git 启用后 backup/rollback-version 的引导回复）。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeprecatedResult {
    /// 恒为 false
    pub success: bool,
    /// 废弃标记
    pub deprecated: bool,
    /// 引导文案（指向 /api/git 等替代接口）
    pub message: String,
}
