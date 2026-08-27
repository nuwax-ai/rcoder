//! 响应载荷 DTO（HttpResult.data 载荷，原各 handler 文件内联定义）。

use serde::Serialize;

use super::task::BuildTaskStatus;
use file_server::models::KilledPid;

/// build 响应 data（POST /build）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct BuildCreatedData {
    /// 构建任务 ID（轮询 /tasks/{task_id} 与 SSE 订阅用）
    pub task_id: String,
    /// 受理时状态（恒为 pending——异步任务已创建；与 /tasks/{task_id} 轮询共用
    /// BuildTaskStatus 状态机，序列化值 "pending"）
    pub status: BuildTaskStatus,
    /// 预生成的产物相对路径（`builds/workspace-package-{release_id}.zip`，release_id
    /// 创建时即生成）——信息字段：标识本次构建的产物位置；实际取包按 app 直下
    /// `GET /api/v1/userapp/static/{app_id}`（缺省最新产物；带 `?release_id=` 精确
    /// 取本版本，回滚/比对指定版本用）。
    pub artifact_path: String,
}

/// cancel 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
pub struct CancelData {
    /// 被取消的任务 ID
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<BuildTaskStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_terminal: Option<bool>,
}

/// detect 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
pub struct DetectData {
    pub detection: DetectionResult,
}

/// confirm 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
pub struct ConfirmData {
    pub path: String,
}

/// detect_project 检测结果（原 service/userapp/import.rs 内联定义）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DetectionResult {
    pub project_dir: String,
    pub detected_type: String,
    pub draft_path: String,
    pub manifest: String,
    pub warnings: Vec<String>,
}

/// ensure-workspace 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappEnsureWorkspaceData {
    /// 建好的 workspace 绝对路径（容器内视角，`{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub workspace: String,
}

/// dev/stop 响应 data（POST /dev/stop）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappDevStopped {
    /// 停止结果消息（"Stopped" / "No running process found" / "Partially stopped but continue execution"）
    pub message: String,
    /// 应用 ID
    pub app_id: String,
    /// 被停进程 ID（按 app_id 定位进程组，无需 pid，恒为 null）
    pub pid: Option<u32>,
    /// 被杀进程 PID 明细（killed 标记是否杀灭成功）
    pub killed_pids: Vec<KilledPid>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappDevProcess {
    /// 应用 ID
    pub app_id: String,
    /// 主进程 PID
    pub pid: u32,
    /// 服务端口（UserApp workspace 恒为 pingap 9080 统一入口）
    pub port: u16,
    /// 启动时间（Unix 毫秒）
    pub started_at: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappDevList {
    /// 在跑的 UserApp 开发服务列表（不含 web/computer 项目进程）
    pub list: Vec<UserappDevProcess>,
}

/// dev 异步任务受理响应 data（POST /dev/start、/dev/restart——编译+启停）。
/// 字段 snake_case 对齐 BuildCreatedData（Java 同一消费面）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappDevTaskCreated {
    /// 应用 ID
    pub app_id: String,
    /// 异步任务 ID（轮询 /api/v1/userapp/tasks/{task_id}、SSE /api/v1/userapp/tasks/{task_id}/logs/stream）
    pub task_id: String,
    /// 受理时状态（恒为 pending——后台任务已创建；与 /tasks/{task_id} 轮询共用
    /// BuildTaskStatus 状态机，序列化值 "pending"）
    pub status: BuildTaskStatus,
}
