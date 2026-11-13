//! Computer Agent HTTP API 类型定义
//!
//! 这些类型用于 Computer Agent 的 HTTP REST API，
//! 由 rcoder 和 agent_runner 共享使用

use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::{Attachment, ChatAgentConfig, ModelProviderConfig, ServiceType};

/// Computer Agent 聊天请求
///
/// 与标准 ChatRequest 的主要区别：
/// - `user_id` 是必填字段（用于容器标识）
/// - Computer 容器按 user_id 定位；UserApp builder 按 app_id 定位。
///   同一容器内的不同 project_id 对应独立 Agent 实例。
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct ComputerChatRequest {
    /// 用户 ID（必填），用于会话归属；UserApp 容器和源码目录由 app_id 定位。
    #[schema(example = "user_123")]
    pub user_id: String,

    /// Agent 会话身份（可选）；userApp 的不同 project_id 独立执行，
    /// app_id 仅定位共享开发容器和工作目录。未提供时，若 session_id 归属
    /// 可确认则沿用其会话身份，否则生成 UUID，并在本轮请求及内部重试中保持一致。
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 业务域路由标记——**复用容器族 [`ServiceType`]**（单一词表单一事实源：
    /// 手写宽容反序列化走 `ServiceType::from_str`，与 pod 族接口同一输入面，
    /// Java 出站统一词表，杜绝两族词表漂移）。chat 域合法路由（受理层
    /// fail-fast 收窄，词表外的容器形态显式拒绝——路由错容器比 400 更难排查）：
    ///
    /// - `Userapp`（含 `userapp`/`user-app`/`application`/`app` 同义词）→
    ///   userApp 开发对话：路由到该 app（app_id 定位）的 UserappBuilder
    ///   开发容器，ACP agent 直接在开发卷 workspace（`{USERAPP_WORKSPACE_DIR}/{app_id}`）
    ///   上工作，生成的代码直接落卷。仅开发阶段传；部署后无对话
    /// - `ComputerNormalProject` → 常规项目：与 ComputerAgentRunner 复用同一
    ///   per-user 容器，agent 工作目录缺省推导 `/home/user/normalProject/{project_id}`
    /// - 缺省（不传）/ `ComputerAgentRunner` → 普通 computer 沙箱对话
    /// - `WebAgentRunner` / `UserappBuilder` → chat 域无此路由，受理层显式拒绝
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(inline)] // 枚举描述直读字段列表（否则 $ref 藏进组件库不直观）
    pub service_type: Option<ServiceType>,

    /// userApp 应用 ID——userApp 开发对话场景（service_type=userapp）必填，
    /// 定位该 app 的 UserappBuilder 开发容器（标识 = app_id）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "app_001")]
    pub app_id: Option<String>,

    /// userApp 应用阶段 dev/prod（缺省 dev）——userApp 开发对话仅支持
    /// dev：agent 会话只存在于 UserappBuilder 开发容器，prod 运行容器
    /// 无 agent 会话（形态对齐 agent 族五接口与 pod 接口族）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,

    /// 用户输入的 prompt
    #[schema(example = "帮我打开浏览器访问 https://example.com")]
    pub prompt: String,

    /// 可选的会话 ID，如果不提供则创建新会话
    #[schema(example = "session789")]
    pub session_id: Option<String>,

    /// 可选的附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,

    /// 数据源附件列表 - 用于AI开发时获取外部数据源信息
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_source_attachments: Vec<String>,

    /// 模型配置
    pub model_provider: Option<ModelProviderConfig>,

    /// 可选的请求ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "req_123456789")]
    pub request_id: Option<String>,

    /// 可选的系统提示词覆盖
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,

    /// 可选的用户提示词模板
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,

    /// Agent 运行时配置
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<ChatAgentConfig>,

    // === 新增字段 (v2 - 隔离类型支持) ===
    /// 容器唯一标识，若传值则使用此 ID 标识容器，实现容器复用
    /// 若不传则使用 user_id 作为容器标识（保持原有逻辑）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    /// 当 pod_id 有值时，此字段必须非空
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    /// 当 pod_id 有值时，此字段必须非空
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    /// 可选值：tenant（租户隔离）、space（空间隔离）、project（项目隔离，默认）
    /// 当 pod_id 有值时，此字段必须非空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// Agent 工作目录（可选，替代 project_id 参与工作目录路径拼接）。两形态：
    ///
    /// 1. **单段目录名**（原语义）：容器内工作目录为 `/home/user/{agent_work_dir}`，
    ///    未提供时使用 project_id
    /// 2. **绝对路径**（常规项目场景，跨平台字符串规则：POSIX `/a/b`、Windows
    ///    盘符 `X:/a/b`、UNC `//s/a`，识别并原样透传 Windows verbatim
    ///    `\\?\C:\a\b` / `\\?\UNC\s\a` 扩展长度路径；均拒 `.`/`..` 段）：Java 传
    ///    子容器内用户维度路径 `/home/user/{projectType}/{projectId}`，agent 以此
    ///    为会话 cwd。仅默认（用户维度）隔离支持——pod_id/tenant/space 与之组合
    ///    会被拒绝；`/home/user` 前缀会在主容器挂载卷预创建对应目录，其余前缀
    ///    由容器内创建
    ///
    /// 📋 Java 配套契约：chat 传绝对 agent_work_dir 后，后续 `/api/computer/*`
    /// 文件族接口（get-logs、execute-command 等）须以 `workspacePath`
    /// （body/query 或 `x-workspace-path` header）传同一绝对路径，`cid` 保持单段
    /// （cid 通道拒 `/`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "/home/user/web/proj_001")]
    pub agent_work_dir: Option<String>,
}

/// Computer Agent 状态查询请求
#[derive(Debug, Clone, Deserialize, Serialize, Validate, ToSchema)]
pub struct ComputerAgentStatusRequest {
    /// 用户 ID（可与 pod_id 二选一）
    #[garde(skip)]
    #[serde(default)]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 调用方会话/project ID（Computer 路径必填）。UserApp 控制保留此身份，
    /// 按持久会话绑定定位共享 builder；app_id 不替代 agent 会话身份。
    #[garde(required, length(min = 1))]
    #[serde(default)]
    #[schema(example = "project_456")]
    pub project_id: Option<String>,

    /// Optional exact session identity. Omitted queries the current project agent;
    /// provided queries only that session and cannot report a replacement as alive.
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "session_789")]
    pub session_id: Option<String>,

    /// Pod ID，用于共享容器模式下的容器定位（可与 user_id 二选一）
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 租户ID（可选）
    #[garde(skip)]
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间ID（可选）
    #[garde(skip)]
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "space_001")]
    pub space_id: Option<String>,

    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,

    /// userApp 分派标记。可选值：`userapp`（推荐）、`user-app` / `application` /
    /// `app`（同义变体，均大小写不敏感）——与 app_id 搭配启用分派，定位
    /// UserappBuilder 开发容器，agent 会话仅存在于 dev 阶段。userApp 容器
    /// 类型由 app_stage 推导（dev=UserappBuilder / prod=Userapp），**勿传
    /// `user-app-builder`**。省略时先按会话绑定识别 UserApp，未绑定再走既有 Computer 路径
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "userapp")]
    pub service_type: Option<String>,

    /// userApp 应用 ID——存在即进入 userApp 分派（定位 UserappBuilder
    /// 开发容器，其标识 = app_id）；本接口分派仅支持 dev 阶段
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "app_001")]
    pub app_id: Option<String>,

    /// userApp 应用阶段 dev/prod（缺省 dev）——本接口 userApp 分派仅支持
    /// dev：agent 会话只存在于 UserappBuilder 开发容器，prod 运行容器
    /// 无 agent 会话
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
}

/// Computer Agent 状态查询响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerAgentStatusResponse {
    /// 用户 ID（可选，因为请求中 user_id 和 pod_id 二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 项目 ID
    #[schema(example = "project_456")]
    pub project_id: String,

    /// Agent 是否存活
    #[schema(example = true)]
    pub is_alive: bool,

    /// 会话 ID（仅当 is_alive=true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "session_789")]
    pub session_id: Option<String>,

    /// Agent 状态（仅当 is_alive=true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "idle")]
    pub status: Option<String>,

    /// 最后活跃时间（仅当 is_alive=true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<chrono::DateTime<chrono::Utc>>,

    /// 创建时间（仅当 is_alive=true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,

    /// agent 进程组 CPU 使用率（0-100，多核/多进程可超 100）。
    /// None = 首次查询（无差分基线）/ 采样间隔不足 / 采集降级，稍后轮询即有值；
    /// is_alive=false 时恒为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 12.5)]
    pub cpu_percent: Option<f64>,

    /// agent 进程组内存占用（MB，不含容器内 supervisord/VNC 等其他进程）。
    /// None = agent 未运行、平台不支持（非 Linux）或 /proc 读取失败（降级不报错）；
    /// is_alive=false 时恒为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 256)]
    pub memory_mb: Option<i64>,
}

impl ComputerAgentStatusResponse {
    /// 创建 Agent 未启动的响应
    pub fn not_alive(user_id: Option<String>, project_id: String) -> Self {
        Self {
            user_id,
            project_id,
            is_alive: false,
            session_id: None,
            status: None,
            last_activity: None,
            created_at: None,
            cpu_percent: None,
            memory_mb: None,
        }
    }
}

/// Computer Agent 停止请求
#[derive(Debug, Clone, Deserialize, Serialize, Validate, ToSchema)]
pub struct ComputerAgentStopRequest {
    /// 用户 ID（可与 pod_id 二选一）
    #[garde(skip)]
    #[serde(default)]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 调用方会话/project ID（Computer 路径必填）。UserApp 按持久会话绑定
    /// 控制独立 agent；app_id 只定位共享 builder，不替代此字段。
    #[garde(required, length(min = 1))]
    #[serde(default)]
    #[schema(example = "project_456")]
    pub project_id: Option<String>,

    /// 可选的会话 ID
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "session789")]
    pub session_id: Option<String>,

    /// Pod ID，用于共享容器模式下的容器定位（可与 user_id 二选一）
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 租户ID（可选）
    #[garde(skip)]
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间ID（可选）
    #[garde(skip)]
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "space_001")]
    pub space_id: Option<String>,

    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,

    /// userApp 分派标记。可选值：`userapp`（推荐）、`user-app` / `application` /
    /// `app`（同义变体，均大小写不敏感）——与 app_id 搭配启用分派，定位
    /// UserappBuilder 开发容器，agent 会话仅存在于 dev 阶段。userApp 容器
    /// 类型由 app_stage 推导（dev=UserappBuilder / prod=Userapp），**勿传
    /// `user-app-builder`**。省略时先按会话绑定识别 UserApp，未绑定再走既有 Computer 路径
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "userapp")]
    pub service_type: Option<String>,

    /// userApp 应用 ID——存在即进入 userApp 分派（定位 UserappBuilder
    /// 开发容器，其标识 = app_id）；本接口分派仅支持 dev 阶段
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "app_001")]
    pub app_id: Option<String>,

    /// userApp 应用阶段 dev/prod（缺省 dev）——本接口 userApp 分派仅支持
    /// dev：agent 会话只存在于 UserappBuilder 开发容器，prod 运行容器
    /// 无 agent 会话
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
}

/// Computer Agent 停止响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerAgentStopResponse {
    /// 是否成功
    #[schema(example = true)]
    pub success: bool,

    /// 结果消息
    #[schema(example = "Agent stopped successfully")]
    pub message: String,

    /// 用户 ID（可选，因为请求中 user_id 和 pod_id 二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// Pod ID（可选，因为请求中 user_id 和 pod_id 二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 项目 ID
    #[schema(example = "project_456")]
    pub project_id: String,
}

/// Computer Agent 重启响应
///
/// 重启只停止并重建 agent 执行，不销毁容器；与 stop 的差异是保留会话注册，
/// 同一 project 的下次对话自动尝试恢复上下文（恢复失败自动降级新会话）。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerAgentRestartResponse {
    /// 是否成功（agent 未运行时也返回成功——目标态已达成，幂等语义）
    #[schema(example = true)]
    pub success: bool,

    /// 结果消息
    #[schema(example = "Agent 已重启，下次对话将尝试恢复上下文")]
    pub message: String,

    /// 用户 ID（可选，因为请求中 user_id 和 pod_id 二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// Pod ID（可选，因为请求中 user_id 和 pod_id 二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 项目 ID
    #[schema(example = "project_456")]
    pub project_id: String,

    /// 重启后保留的可恢复会话 ID；None 表示该 project 没有可恢复的会话
    /// （此前无成功对话），下次对话将是全新会话
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "session789")]
    pub resumed_session_id: Option<String>,
}

/// Computer 容器(整个 pod)资源用量响应
///
/// 统计口径:容器 cgroup v2(agent/VNC/桌面全部进程),与
/// `/computer/agent/status` 的 agent 进程组粒度互补。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerPodResourcesResponse {
    /// 用户 ID(可选)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// Pod ID(可选)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 实际解析到的业务场景(回显)。可选值：`WebAgentRunner` /
    /// `ComputerAgentRunner` / `ComputerNormalProject` / `Userapp` /
    /// `UserappBuilder`。
    ///
    /// 同一个标识符在不同容器族下指向不同容器，调用方应以此字段确认自己看的
    /// 是哪一个族，不要按 `container_name` 前缀猜。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<ServiceType>,

    /// 容器是否存活(true=存在且运行中)
    #[schema(example = true)]
    pub alive: bool,

    /// 整容器 CPU 使用率(0 起,多核可超 100;查询驱动差分)。
    /// None = 首次查询(无基线)/ 采样间隔不足 / 采集降级
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 35.0)]
    pub container_cpu_percent: Option<f64>,

    /// 整容器内存占用(MB,cgroup memory.current;含 agent/VNC/桌面全部进程)。
    /// None = 采集降级(不报错)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1080)]
    pub container_memory_mb: Option<i64>,

    /// 容器内存限额(MB,容器创建时的 resource_limits;未设限为 None)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 3954)]
    pub container_memory_limit_mb: Option<i64>,

    /// 容器 CPU 核数限额(容器创建时的 resource_limits;未设限为 None)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 2.0)]
    pub container_cpu_limit_cores: Option<f64>,

    /// agent_runner 进程存活时长(秒)
    #[schema(example = 3600)]
    pub uptime_seconds: i64,

    /// 容器 ID(定位成功时)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "abc123def456")]
    pub container_id: Option<String>,

    /// 容器内进程明细(按 RSS 降序;include_processes=false 或采集降级时缺省)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processes: Option<Vec<ComputerProcessEntry>>,

    /// 进程总数(容器内全部进程)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_count: Option<u32>,

    /// 角色分组聚合(与内存看门狗淘汰语义对齐)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_groups: Option<ComputerProcessGroups>,

    /// 容器内**所有** agent 执行的进程组聚合(仅 include_processes 时填充)。
    /// 一个 user 容器可并存多个 project，每个一条；无注册执行时为空。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<ComputerAgentExecution>,

    /// 容器名称(定位成功时)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "computer-agent-runner-user_123")]
    pub container_name: Option<String>,
}

/// 容器内单进程明细(资源观测)
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerProcessEntry {
    /// 进程 ID(容器 pid namespace)
    pub pid: u32,
    /// 进程名(内核 comm,≤15 字符)
    pub comm: String,
    /// RSS(MB)
    pub rss_mb: f64,
    /// 累计 CPU 时间(秒,utime+stime;前端按采样差分得 CPU%)
    pub cpu_seconds: f64,
    /// 角色分组: infra | platform | workload
    pub group: String,
    /// 命令行前缀(≤120 字符)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_head: Option<String>,
}

/// 进程角色分组聚合(字节口径,前端按需换算 MB)
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerProcessGroups {
    /// 桌面/系统(永不淘汰)
    pub infra_rss_bytes: u64,
    /// 平台服务(claude/MCP/file-server,永不淘汰)
    pub platform_rss_bytes: u64,
    /// 会话工作负载(看门狗可淘汰范围)
    pub workload_rss_bytes: u64,
}

/// Computer 容器资源查询参数(GET query)
#[derive(Debug, Clone, Deserialize, IntoParams, ToSchema)]
pub struct ComputerPodResourcesQuery {
    /// 用户 ID(与 pod_id 二选一)
    #[serde(default)]
    #[param(example = "user_123")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// Pod/容器标识符(与 user_id 二选一)
    #[serde(default)]
    #[param(example = "pod_abc123")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 业务场景(容器族)。缺省 `computer-agent-runner`。可选值：
    /// `WebAgentRunner` / `ComputerAgentRunner` / `ComputerNormalProject` /
    /// `Userapp` / `UserappBuilder`。
    ///
    /// **与 `/computer/pod/ensure` 同一套三字段分派**（`service_type` +
    /// `app_id` + `app_stage`，共用 `parse_app_target`，不另立规则）：
    ///
    /// - `computer-agent-runner` / `computer-normal-project`：同一容器族
    ///   （共容器、共镜像、共 PVC），标识符是 `user_id`；
    /// - `userapp`（或省略 service_type）+ `app_id`：按 `app_stage` 分派，
    ///   `dev`（缺省）→ UserappBuilder 开发容器（有 agent_runner，可返回明细），
    ///   `prod` → 业务运行 Deployment（无 agent_runner，返回
    ///   `ERR_OPERATION_NOT_SUPPORTED`）；
    /// - `web-agent-runner`：标识符是 `project_id`。
    #[serde(default)]
    #[param(example = "userapp")]
    #[schema(example = "userapp")]
    pub service_type: Option<ServiceType>,

    /// userApp 应用 ID（与 `service_type=userapp` 搭配；存在即走 userApp 分派）。
    /// 语义与 `/computer/pod/ensure` 完全一致。
    #[serde(default)]
    #[param(example = "app_789")]
    #[schema(example = "app_789")]
    pub app_id: Option<String>,

    /// userApp 容器阶段（缺省 `dev`）：`dev` = UserappBuilder 开发容器；
    /// `prod` = 业务运行 Deployment（本接口不支持，无 agent_runner）。
    #[serde(default)]
    #[param(example = "dev")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,

    /// 项目 ID：`web-agent-runner` 族的容器标识符（其余族不使用）。
    #[serde(default)]
    #[param(example = "proj_456")]
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 租户 ID（可选，仅创建期隔离元数据）。
    ///
    /// **不参与容器定位**：容器标识只由 `user_id` / `project_id` / `app_id`
    /// 决定，而一租户下的同一 user 在 computer 族里只有一台容器（名字由
    /// identifier 拼），所以传入它找到的是同一台容器。
    ///
    /// 之所以仍然接受，是为了和 `/computer/pod/ensure` 的入参面对齐——调用方
    /// 可以把创建时的请求参数原样带过来，不必删字段。传入值只用于日志留痕。
    /// 若将来需要"按租户过滤/隔离查询"，那是查找层要支持的新能力，不是加个
    /// 参数就能生效的。
    #[serde(default)]
    #[param(example = "tenant_abc")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID（可选，仅创建期隔离元数据；语义同 `tenant_id`，不参与定位）。
    #[serde(default)]
    #[param(example = "space_xyz")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型（可选，仅创建期隔离元数据；语义同 `tenant_id`，不参与定位）。
    #[serde(default)]
    #[param(example = "tenant")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 是否返回容器内进程明细(默认 true;响应 processes 字段)
    #[serde(default = "computer_pod_resources_default_include_processes")]
    #[param(example = true)]
    #[schema(example = true)]
    pub include_processes: Option<bool>,

    /// SSE 采样周期(秒,默认 2,clamp 1-30)。仅 `/stream` 端点使用。
    #[serde(default)]
    #[param(example = 2, minimum = 1, maximum = 30)]
    #[schema(example = 2)]
    pub interval_secs: Option<u64>,
}

fn computer_pod_resources_default_include_processes() -> Option<bool> {
    Some(true)
}

/// SSE 采样周期的默认值(秒)。
pub const COMPUTER_POD_RESOURCES_DEFAULT_INTERVAL_SECS: u64 = 2;

/// SSE 采样周期的合法区间(秒):下限防止把 agent_runner 打成 DoS,
/// 上限防止连接挂着却几乎不采样。
pub const COMPUTER_POD_RESOURCES_MIN_INTERVAL_SECS: u64 = 1;
pub const COMPUTER_POD_RESOURCES_MAX_INTERVAL_SECS: u64 = 30;

/// 归一化 SSE 采样周期:非法值回落到默认,越界夹到区间内。
pub fn computer_pod_resources_clamp_interval(raw: Option<u64>) -> u64 {
    raw.filter(|v| *v > 0)
        .unwrap_or(COMPUTER_POD_RESOURCES_DEFAULT_INTERVAL_SECS)
        .clamp(
            COMPUTER_POD_RESOURCES_MIN_INTERVAL_SECS,
            COMPUTER_POD_RESOURCES_MAX_INTERVAL_SECS,
        )
}

/// 容器内 agent 执行的进程组聚合（`/computer/pod/resources` 提供）。
///
/// 观测端只用新接口就能拿到 agent 维度数据，不必再调
/// `/computer/agent/status`——那个接口的 `cpu_percent` 是**服务端差分**，
/// 外部 2s 轮询会改写它的基线，让平台自身查询拿到 None 或失真值。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerAgentExecution {
    /// 该执行所属 project（同一 user 容器可并存多个 project）
    pub project_id: String,
    /// 当前执行的会话 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 执行状态：Processing / Idle / Terminating
    pub status: String,
    /// agent 进程组 RSS 合计（MB，含其 MCP 子进程）
    pub rss_mb: f64,
    /// agent 进程组**累计** CPU 秒（utime+stime）。
    /// 消费方自行差分算百分比：累计值无状态，任意多个消费者互不影响。
    pub cpu_seconds: f64,
    /// agent 进程组内进程数
    pub process_count: u32,
}

/// Computer Agent 取消任务的请求参数
#[derive(Debug, Clone, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct ComputerAgentCancelRequest {
    /// 用户 ID（可与 pod_id 二选一）
    #[param(example = "user_123")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 调用方会话/project ID（Computer 路径必填，服务端校验非空）。
    /// UserApp 按会话绑定控制独立 agent，app_id 只定位共享 builder；
    /// 不携带 app_id 的 user_id/project_id/session_id 请求也可由绑定解析。
    #[serde(default)]
    #[param(example = "project_456")]
    #[schema(example = "project_456")]
    pub project_id: Option<String>,

    /// 会话 ID（可选，未提供时从 registry 查找）
    #[param(example = "session_789")]
    #[schema(example = "session_789")]
    #[serde(default)]
    pub session_id: Option<String>,

    /// Pod ID，用于共享容器模式下的容器定位（可与 user_id 二选一）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "pod_abc123")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 租户ID（可选）
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[param(example = "tenant_001")]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间ID（可选）
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[param(example = "space_001")]
    #[schema(example = "space_001")]
    pub space_id: Option<String>,

    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "project")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,

    /// userApp 分派标记。可选值：`userapp`（推荐）、`user-app` / `application` /
    /// `app`（同义变体，均大小写不敏感）——与 app_id 搭配启用分派，定位
    /// UserappBuilder 开发容器，agent 会话仅存在于 dev 阶段。userApp 容器
    /// 类型由 app_stage 推导（dev=UserappBuilder / prod=Userapp），**勿传
    /// `user-app-builder`**。省略时先按会话绑定识别 UserApp，未绑定再走既有 Computer 路径
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "userapp")]
    #[schema(example = "userapp")]
    pub service_type: Option<String>,

    /// userApp 应用 ID——存在即进入 userApp 分派（定位 UserappBuilder
    /// 开发容器，其标识 = app_id）；本接口分派仅支持 dev 阶段
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "app_001")]
    #[schema(example = "app_001")]
    pub app_id: Option<String>,

    /// userApp 应用阶段 dev/prod（缺省 dev）——本接口 userApp 分派仅支持
    /// dev：agent 会话只存在于 UserappBuilder 开发容器，prod 运行容器
    /// 无 agent 会话
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "dev")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
}

/// Computer Agent 取消响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerAgentCancelResponse {
    /// 是否成功
    #[schema(example = true)]
    pub success: bool,

    /// 会话 ID
    #[schema(example = "session_789")]
    pub session_id: String,
}

#[cfg(test)]
mod chat_service_type_tests {
    use super::ComputerChatRequest;
    use crate::ServiceType;

    /// chat `service_type` 复用容器族 `ServiceType`（单一词表）：反序列化走
    /// `ServiceType::from_str` 宽容词表，与 pod 族接口同一输入面；序列化输出
    /// 为 ServiceType 规范词（kebab-case，gRPC 转发链同词表）。
    ///
    /// 历史：曾独立 `ChatServiceScope` 枚举（serde 严格词表 userapp/normalProject），
    /// Java 出站统一取 ServiceType 词（pod 族一直放行），09-19 NormalProject
    /// 会话传 `computer-normal-project` 在 chat 反序列化即拒 → axum 层 400
    /// INVALID_PARAMS（请求不进 handler）。统一后单一词表杜绝此类漂移。
    #[test]
    fn chat_service_type_uses_service_type_vocabulary() {
        // Userapp 同义词 → Userapp（含 header 契约规范词 "userapp"）
        for word in [
            "userapp",
            "user-app",
            "UserApp",
            "Userapp",
            "application",
            "app",
        ] {
            assert_eq!(
                serde_json::from_str::<ServiceType>(&format!("\"{word}\""))
                    .unwrap_or_else(|e| panic!("{word} 应可反序列化: {e}")),
                ServiceType::Userapp
            );
        }
        // NormalProject 词族 → ComputerNormalProject（含 ChatServiceScope 旧
        // 契约的 camelCase 词 normalProject——ServiceType FromStr 保留）
        for word in [
            "computer-normal-project",
            "ComputerNormalProject",
            "normalProject",
        ] {
            assert!(
                serde_json::from_str::<ServiceType>(&format!("\"{word}\"")).is_ok(),
                "{word} 应可反序列化"
            );
        }
        assert_eq!(
            serde_json::from_str::<ServiceType>("\"computer-normal-project\"").unwrap(),
            ServiceType::ComputerNormalProject
        );

        // 词表外的值仍然反序列化即拒（fail-fast 不静默回落）
        assert!(serde_json::from_str::<ServiceType>("\"unknown-type\"").is_err());
    }

    /// 事故现场形态回归：Java 新版 NormalProject 会话的真实 body——
    /// `service_type:"computer-normal-project"` 必须完整反序列化通过。
    #[test]
    fn chat_request_accepts_normal_project_kebab_wire() {
        let chat: ComputerChatRequest = serde_json::from_str(
            r#"{"user_id":"6","project_id":"1693664","service_type":"computer-normal-project",
                "agent_work_dir":"262","prompt":"hi"}"#,
        )
        .unwrap();
        assert_eq!(chat.service_type, Some(ServiceType::ComputerNormalProject));
    }
}

#[cfg(test)]
mod userapp_dispatch_tests {
    use super::*;

    /// 契约钉住：agent 族接口（status/stop/cancel）的 userApp wire 形态 =
    /// service_type=userapp + app_id + app_stage（可缺省），
    /// app_id 定位共享 builder，project_id 保留独立会话身份；
    /// Java 仅传 user_id/project_id 时也可由持久绑定定位。
    #[test]
    fn agent_requests_deserialize_userapp_wire_form() {
        let raw = r#"{"service_type":"userapp","app_id":"app-1","app_stage":"dev"}"#;
        let status: ComputerAgentStatusRequest =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("status {raw} 应可反序列化: {e}"));
        assert_eq!(status.service_type.as_deref(), Some("userapp"));
        assert_eq!(status.app_id.as_deref(), Some("app-1"));
        assert!(status.project_id.is_none());
        assert_eq!(status.app_stage.as_deref(), Some("dev"));

        let stop: ComputerAgentStopRequest =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("stop {raw} 应可反序列化: {e}"));
        assert_eq!(stop.app_id.as_deref(), Some("app-1"));

        // cancel 最小形态：仅 app_id（project_id 可不传——computer 语义字段）
        let cancel_raw = r#"{"service_type":"userapp","app_id":"app-1"}"#;
        let cancel: ComputerAgentCancelRequest = serde_json::from_str(cancel_raw)
            .unwrap_or_else(|e| panic!("cancel {cancel_raw} 应可反序列化: {e}"));
        assert_eq!(cancel.app_id.as_deref(), Some("app-1"));
        assert!(cancel.project_id.is_none());
        assert!(cancel.app_stage.is_none());
    }

    /// 契约钉住：/computer/chat 的 userApp wire 形态（service_type 枚举 +
    /// app_id 定位 + app_stage 可缺省）——app_stage 与 agent 族
    /// 五接口同语义（缺省 dev，prod 服务端 400）。
    #[test]
    fn chat_request_deserializes_userapp_wire_form_with_app_stage() {
        for raw in [
            r#"{"service_type":"userapp","app_id":"app-1","prompt":"hi","user_id":"u1"}"#,
            r#"{"service_type":"userapp","app_id":"app-1","app_stage":"dev","prompt":"hi","user_id":"u1"}"#,
        ] {
            let chat: ComputerChatRequest = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("chat userApp 形态 {raw} 应可反序列化: {e}"));
            assert_eq!(chat.app_id.as_deref(), Some("app-1"));
            assert!(
                chat.app_stage.as_deref().is_none_or(|s| s == "dev"),
                "app_stage 应可缺省或为 dev"
            );
        }
        // 非法 stage 是服务端校验（String 承接，非 serde 枚举）——反序列化应通过
        let chat: ComputerChatRequest = serde_json::from_str(
            r#"{"service_type":"userapp","app_id":"app-1","app_stage":"prod","prompt":"hi","user_id":"u1"}"#,
        )
        .unwrap();
        assert_eq!(chat.app_stage.as_deref(), Some("prod"));
    }

    /// 既有 computer 形态回归：不传三字段反序列化不受影响（全部缺省）。
    #[test]
    fn agent_requests_deserialize_legacy_computer_form() {
        let raw = r#"{"user_id":"user_123","project_id":"project_456"}"#;
        let status: ComputerAgentStatusRequest = serde_json::from_str(raw).unwrap();
        assert!(status.service_type.is_none());
        assert!(status.app_stage.is_none());
        let stop: ComputerAgentStopRequest = serde_json::from_str(raw).unwrap();
        assert!(stop.service_type.is_none());
    }
}
