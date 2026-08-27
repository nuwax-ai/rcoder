//! 响应载荷 DTO（原 handlers 与 service 层内联定义）。
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
    pub status: String,
    pub timestamp: i64,
    pub uptime: u64,
    pub version: String,
    pub platform: String,
    pub node_version: String,
    pub pid: u32,
    pub memory: MemoryUsage,
    pub env: String,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryUsage {
    pub rss: f64,
    pub heap_used: f64,
    pub heap_total: f64,
    pub external: f64,
}

/// `/api/version` 响应 (对齐 TS `{ success: true, version }`)。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct VersionResponse {
    pub success: bool,
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
    pub line: usize,
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
