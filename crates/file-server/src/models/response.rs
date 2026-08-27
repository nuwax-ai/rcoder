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
    pub url: String,
    pub error: String,
}

/// create-workspace 响应 (对齐 nuwax createWorkspace 响应字段)。
/// workspaceRoot = COMPUTER_WORKSPACE_DIR; updatedSkills/failedSkills 空时不输出。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceResponse {
    pub success: bool,
    pub message: String,
    pub workspace_root: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub updated_skills: Vec<String>,
    /// best-effort 透传: 推送失败的 skill URL 明细 (空则不输出)。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed_skills: Vec<SkillFailure>,
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
    #[serde(skip)]
    pub log_dir: std::path::PathBuf,
    #[serde(skip)]
    pub temp_log_name: String,
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
