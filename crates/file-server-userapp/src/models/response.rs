//! 响应载荷 DTO（HttpResult.data 载荷）。

use serde::Serialize;

use super::task::BuildTaskStatus;
use file_server::models::KilledPid;

/// build 响应 data（POST /build）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct BuildCreatedData {
    /// 构建任务 ID（轮询 /tasks/{task_id} 与 SSE 订阅用）
    pub task_id: String,
    /// 受理时状态（恒为 `pending`——异步任务已创建；与 /tasks/{task_id} 轮询共用
    /// BuildTaskStatus 状态机：`pending` / `running` / `completed` / `failed` / `cancelled`）
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
    /// 取消后任务状态（枚举：`pending` / `running` / `completed` / `failed` / `cancelled`）
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

/// detect_project 检测结果（service::userapp::import 的返回值）。
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
    /// 建好的 workspace 绝对路径（容器内视角，`{USERAPP_WORKSPACE_DIR}/{app_id}`）
    pub workspace: String,
}

/// 文件列表/搜索条目（userapp 域 snake wire；共享 `tree::FileEntry` 的
/// camelCase serde 是 computer 域 TS 契约，经 [`From`] 转换，缺省字段的
/// skip 语义保持一致）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappFileEntry {
    /// 文件/目录名
    pub name: String,
    /// 是否目录
    pub is_dir: bool,
    /// 是否二进制（文本预读时的判定；缺省省略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    /// 内容超限未读（缺省省略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_exceeded: Option<bool>,
    /// 文本内容（预读窗口内；缺省省略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contents: Option<String>,
    /// 预览 URL（proxy_path 提供时；缺省省略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_proxy_url: Option<String>,
    /// 是否软链（缺省省略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_link: Option<bool>,
}

impl From<file_server::service::tree::FileEntry> for UserappFileEntry {
    fn from(e: file_server::service::tree::FileEntry) -> Self {
        Self {
            name: e.name,
            is_dir: e.is_dir,
            binary: e.binary,
            size_exceeded: e.size_exceeded,
            contents: e.contents,
            file_proxy_url: e.file_proxy_url,
            is_link: e.is_link,
        }
    }
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
    /// 服务端口（Userapp workspace 恒为 pingap 9080 统一入口）
    pub port: u16,
    /// 启动时间（Unix 毫秒）
    pub started_at: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappDevList {
    /// 在跑的 Userapp 开发服务列表（不含 web/computer 项目进程）
    pub list: Vec<UserappDevProcess>,
}

// ── workspace 框架识别（GET /dev/framework-info）─────────────────────────────

/// 单维度框架识别结果（build 与 ui 两维度同构）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappFrameworkDetection {
    /// 稳定标识：build 维度 `nextjs`/`nuxt`/`remix`/`astro`/`sveltekit`/
    /// `solid-start`/`gatsby`/`docusaurus`/`angular`/`create-react-app`/
    /// `vue-cli`/`rsbuild`/`vite`/`webpack`；ui 维度 `react`/`vue3`/`vue2`/
    /// `vue`/`svelte`/`solid`/`preact`/`angular`；无法识别 `other`
    pub name: String,
    /// 人类可读名（如 "Next.js"/"Vite"/"React"）
    pub display_name: String,
    /// package.json 依赖声明原样（如 "^5.4.21"；未声明为空串）
    pub declared_range: String,
    /// 框架版本（best effort，三级口径精确度递减；提取不到为 null）
    pub version: Option<String>,
    /// 版本来源：`installed`（node_modules 实际安装版本，最准）/
    /// `declared_pinned`（声明为精确版本如 "16.2.12"）/ `declared_range`
    ///（从 "^x.y.z" 提取，仅最低位可信）/ `none`
    pub version_source: String,
}

/// 单服务的识别结果（manifest 声明面 + 探测面）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappServiceFrameworkInfo {
    /// 服务稳定身份（manifest [project].service_id）
    pub service_id: String,
    /// 人类可读服务名（manifest [project].name）
    pub name: String,
    /// 服务语言/形态（manifest [project].type 权威声明：
    /// node/java/python/go/rust/static）
    pub r#type: String,
    /// 服务类别（manifest [project].kind：web/worker）
    pub kind: String,
    /// workspace 内一级子目录名
    pub dir: String,
    /// 是否参与构建/启动（manifest [project].enabled）
    pub enabled: bool,
    /// 包管理器（`pnpm`/`npm`/`yarn`/`bun`：packageManager 字段 > lockfile
    /// 存在性判定；无 package.json 的非 Node 服务为 null）
    pub package_manager: Option<String>,
    /// 项目使用 TypeScript（typescript 依赖或 tsconfig.json 存在）
    pub typescript: bool,
    /// 构建/meta 框架识别（vite/nextjs/nuxt 等；与 ui 维度正交可同真——
    /// next 项目 build=nextjs 且 ui=react）
    pub build_framework: UserappFrameworkDetection,
    /// UI 框架识别（react/vue3/vue2/svelte 等；vue 细分仅凭 vue 本体主版本）
    pub ui_framework: UserappFrameworkDetection,
}

/// workspace 框架识别响应 data（`GET /dev/framework-info`）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappFrameworkInfo {
    /// 全部服务（含 disabled，enabled 字段自明）的识别结果清单
    pub services: Vec<UserappServiceFrameworkInfo>,
}

/// dev 异步任务受理响应 data（POST /dev/start、/dev/restart——编译+启停）。
/// 字段 snake_case 对齐 BuildCreatedData（Java 同一消费面）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct UserappDevTaskCreated {
    /// 应用 ID
    pub app_id: String,
    /// 异步任务 ID（轮询 /api/v1/userapp/tasks/{task_id}、SSE /api/v1/userapp/tasks/{task_id}/logs/stream）
    pub task_id: String,
    /// 受理时状态（恒为 `pending`——后台任务已创建；与 /tasks/{task_id} 轮询共用
    /// BuildTaskStatus 状态机：`pending` / `running` / `completed` / `failed` / `cancelled`）
    pub status: BuildTaskStatus,
}
