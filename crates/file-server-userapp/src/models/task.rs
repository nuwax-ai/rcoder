//! 构建任务域共享类型。
//!
//! 既是任务状态机核心（TaskState/BuildTask 字段）又是 wire 契约（GET
//! /tasks/{task_id} 直接序列化 BuildTaskSnapshot）。TaskState/BuildTaskStore
//! 留在 service，直接引用本层类型（对齐 app_manager service 依赖 models 的形态）。

use serde::Serialize;

pub type BuildTaskId = String;

/// 任务类型。Build = 发布打包（zip 制品）；DevStart/DevRestart = 开发闭环
/// （manifest 同核编译成功后启动/重启 dev 服务——**启停前必先编译**，新代码
/// 才生效；Completed 的制品四字段为占位空值，调用方按 status/error 消费，
/// 端口经 `GET /api/v1/userapp/dev/list` 查询）。纯开发编译不设接口——与
/// Build 同核无增量，用 `/api/v1/userapp/build`。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildTaskKind {
    Build,
    DevStart,
    DevRestart,
}

/// 任务状态(镜像 app_manager ReleaseStatus 语义)。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BuildTaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 任务快照(GET /tasks/{id} 返回)。
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct BuildTaskSnapshot {
    pub id: BuildTaskId,
    pub app_id: String,
    /// 任务类型（枚举：`build` 发布构建 / `dev_start` 开发启动 / `dev_restart` 开发重启）
    pub kind: BuildTaskKind,
    /// 任务状态（枚举：`pending` / `running` / `completed` / `failed` / `cancelled`）
    pub status: BuildTaskStatus,
    pub stage: Option<String>,
    /// 当前/失败中断的子项目 service_id（构建日志按 service_id 归档，
    /// 失败排查用 `tasks/{id}/logs?service=` 同键取日志）
    pub current_service: Option<String>,
    pub release_id: Option<String>,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
    pub file_name: Option<String>,
    /// 相对 workspace 根的产物路径(`builds/workspace-package-{release_id}.zip`)——
    /// 任务创建时预生成(pending 期即有值),Java 取包 URL 直接拼段。
    pub artifact_path: Option<String>,
    pub error: Option<String>,
    pub seq: u64,
    pub created_at: i64,
    pub updated_at: i64,
}
