# Computer Agent Runner 需求与设计文档

**文档版本**: v1.0
**创建日期**: 2025-12-10
**作者**: Claude (基于用户需求分析)
**项目**: rcoder - AI 驱动开发平台

---

## 一、项目背景

### 1.1 现有架构

rcoder 当前采用的是"一个 project_id 对应一个 Docker 容器"的架构模式：

- **容器类型**: `ServiceType::RCoder`
- **容器命名**: `rcoder-agent-{project_id}`
- **工作目录**: `/app/project_workspace/{project_id}`
- **通信协议**: gRPC (50051) + HTTP (8086)
- **核心功能**: AI 代码生成、项目管理、会话管理

### 1.2 新需求背景

用户希望构建一个带有虚拟远程桌面的 Agentic AI 系统，使 AI Agent 能够：

1. **操作浏览器**: 在虚拟桌面中打开 Chromium，自主搜索和访问网络资料
2. **远程监控**: 用户可通过 VNC 远程查看 Agent 的操作过程
3. **复杂任务处理**: Agent 在容器内完成复杂的多步骤任务（如网页抓取、数据处理等）
4. **资源共享**: 一个用户可以有多个项目，共享同一个桌面环境容器

### 1.3 项目目标

设计并实现 **Computer Agent Runner** 服务，作为 rcoder 的扩展功能模块，具备以下特性：

- ✅ 一个 `user_id` 对应一个带桌面环境的容器
- ✅ 容器内可同时运行多个 `project_id` 对应的 AI Agent 实例
- ✅ 提供 VNC 远程桌面访问，用户可实时查看 Agent 操作
- ✅ 集成 Chrome DevTools MCP，赋予 Agent 浏览器操作能力
- ✅ 智能闲置检测：只有当用户下所有项目都闲置时才销毁容器

---

## 二、需求分析

### 2.1 功能需求

#### FR-1: 用户容器管理

| ID | 需求描述 | 优先级 |
|----|---------|-------|
| FR-1.1 | 系统根据 `user_id` 自动创建和管理容器 | P0 |
| FR-1.2 | 容器命名规则：`computer-agent-runner-{user_id}` | P0 |
| FR-1.3 | 容器挂载路径：`/app/computer-project-workspace/{user_id}` | P0 |
| FR-1.4 | 支持资源限额配置（内存、CPU） | P1 |
| FR-1.5 | 容器启动后自动加载 XFCE 桌面环境和 VNC 服务 | P0 |

#### FR-2: 多 Agent 实例管理

| ID | 需求描述 | 优先级 |
|----|---------|-------|
| FR-2.1 | 容器内支持同时运行多个 `project_id` 对应的 Agent 实例 | P0 |
| FR-2.2 | 每个 Agent 实例独立管理，互不干扰（无上下文污染） | P0 |
| FR-2.3 | 通过 gRPC `Chat` RPC 根据 `project_id` 路由到对应 Agent | P0 |
| FR-2.4 | 支持按 `project_id` 停止单个 Agent（不销毁容器） | P0 |
| FR-2.5 | Agent 使用 `computer_agent_default.json` 配置（包含 Chrome DevTools MCP） | P1 |

#### FR-3: VNC 远程桌面访问

| ID | 需求描述 | 优先级 |
|----|---------|-------|
| FR-3.1 | 提供 HTTP 接口访问 VNC 桌面：`GET /computer/desktop/{user_id}/{project_id}` | P0 |
| FR-3.2 | 通过 Pingora 或 Nginx 透明代理 WebSocket 到容器的 6080 端口 | P0 |
| FR-3.3 | 支持实时桌面查看和交互（通过 noVNC） | P0 |
| FR-3.4 | VNC 连接与 `user_id` 绑定，保障安全隔离 | P1 |

#### FR-4: 浏览器操作能力

| ID | 需求描述 | 优先级 |
|----|---------|-------|
| FR-4.1 | 容器内预装 Chromium 浏览器（远程调试端口 9222） | P0 |
| FR-4.2 | Agent 通过 Chrome DevTools MCP 操作浏览器 | P0 |
| FR-4.3 | 支持网页导航、元素操作、截图等功能 | P1 |

#### FR-5: HTTP 接口

| ID | 接口路径 | 方法 | 说明 |
|----|---------|------|------|
| FR-5.1 | `/computer/chat` | POST | 发送聊天请求（必需 `user_id`） |
| FR-5.2 | `/computer/agent/stop` | POST | 停止特定 `project_id` 的 Agent |
| FR-5.3 | `/computer/progress/{session_id}` | GET | SSE 进度流（同现有 `/agent/progress`） |
| FR-5.4 | `/computer/desktop/{user_id}/{project_id}` | GET | VNC 桌面访问 |

#### FR-6: 闲置检测和资源回收

| ID | 需求描述 | 优先级 |
|----|---------|-------|
| FR-6.1 | 区分 `ServiceType` 的闲置检测策略 | P0 |
| FR-6.2 | `ComputerAgentRunner`：只有当 `user_id` 下所有 `project_id` 都闲置时才销毁容器 | P0 |
| FR-6.3 | 闲置超时时间：默认 30 分钟（可配置） | P1 |
| FR-6.4 | 容器保护期：创建后 5 分钟内不进行清理 | P1 |

### 2.2 非功能需求

| ID | 需求描述 | 指标 |
|----|---------|------|
| NFR-1 | 容器创建时间 | < 30 秒 |
| NFR-2 | VNC 桌面访问延迟 | < 500ms |
| NFR-3 | 单容器并发 Agent 数 | ≥ 3 个 |
| NFR-4 | 内存占用（单容器） | < 4GB（可配置） |
| NFR-5 | CPU 占用（单容器） | < 2 核（可配置） |
| NFR-6 | 系统可用性 | 99.5% |

### 2.3 约束条件

1. **架构约束**：
   - 复用现有 `agent_runner` 模块，通过 `ServiceType::ComputerAgentRunner` 区分
   - 保持与现有 `ServiceType::RCoder` 的兼容性，互不影响

2. **技术约束**：
   - ACP 协议连接不是 `Send` trait，必须在 `LocalSet` 中运行
   - Docker 镜像基于 Debian 12，包含完整桌面环境（2GB+）
   - gRPC 通信端口 50051 不可修改（与 `shared_types` 保持一致）

3. **安全约束**：
   - `user_id` 必须经过身份验证（后续实现）
   - 容器间网络隔离
   - VNC 访问需要与 `user_id` 绑定

---

## 三、系统架构设计

### 3.1 整体架构

```
┌─────────────────────────────────────────────────────────────┐
│                      外部客户端 (HTTP/SSE)                     │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│               RCoder 主服务 (HTTP API Server)                │
│  - Axum 路由: /computer/chat, /computer/agent/stop          │
│  - Pingora 代理: /computer/desktop/{user_id}/{project_id}   │
│  - 状态管理: AppState (DashMap)                              │
└────────────────┬────────────────────────────────────────────┘
                 │
                 │ gRPC (Chat, StopAgent)
                 ▼
┌─────────────────────────────────────────────────────────────┐
│      Docker 容器: computer-agent-runner-{user_id}            │
│                                                               │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  agent_runner gRPC Server (50051)                    │    │
│  │    - ProjectAgentManager                             │    │
│  │    - DashMap<project_id, AgentInstance>              │    │
│  └──────────────┬──────────────────────────────────────┘    │
│                 │                                             │
│                 ▼                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  Agent Instance 1 (project_id_1)                     │    │
│  │    - ACP Agent (claude-code-acp)                     │    │
│  │    - Chrome DevTools MCP                             │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                               │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  Agent Instance 2 (project_id_2)                     │    │
│  │    - ACP Agent (claude-code-acp)                     │    │
│  │    - Chrome DevTools MCP                             │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                               │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  Desktop 环境 (XFCE4 + noVNC)                        │    │
│  │    - Xvfb (:0)                                       │    │
│  │    - x11vnc (5900) → noVNC (6080)                    │    │
│  │    - Chromium (CDP 9222)                             │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                               │
│  工作区: /app/computer-project-workspace/{user_id}/          │
│           ├── project_id_1/                                  │
│           ├── project_id_2/                                  │
│           └── ...                                            │
└───────────────────────────────────────────────────────────────┘
```

### 3.2 容器管理模式对比

| 维度 | RCoder (现有) | Computer Agent Runner (新) |
|------|--------------|---------------------------|
| 容器标识 | `project_id` | `user_id` |
| 容器命名 | `rcoder-agent-{project_id}` | `computer-agent-runner-{user_id}` |
| Agent 实例数 | 1 个 | 多个（按 `project_id` 区分） |
| 工作目录 | `/app/project_workspace/{project_id}` | `/app/computer-project-workspace/{user_id}` |
| 桌面环境 | 无 | XFCE4 + noVNC |
| 浏览器 | 无 | Chromium + CDP |
| 闲置策略 | project_id 闲置即销毁 | user_id 下所有 project_id 都闲置才销毁 |

### 3.3 数据流转

#### 3.3.1 聊天请求流程

```
1. 客户端 → POST /computer/chat
   {
     "user_id": "user_123",
     "project_id": "proj_456",  // 可选
     "prompt": "帮我爬取网页数据"
   }

2. handle_computer_chat()
   ├─ 生成 project_id（若未提供）
   ├─ get_or_create_container_for_user(user_id)
   │  ├─ 检查 containers[ContainerKey::User(user_id)] 是否已存在
   │  ├─ 若不存在，调用 DockerManager 创建容器
   │  └─ 返回 ContainerBasicInfo
   ├─ 更新 UnifiedContainerInfo
   │  ├─ 创建/更新 ProjectInfo
   │  └─ 添加到 container_info.projects
   └─ gRPC Chat RPC → agent_runner

3. agent_runner (gRPC Server)
   ├─ ProjectAgentManager.get_or_create_agent(project_id)
   │  ├─ 检查 agents: DashMap<project_id, AgentInstance>
   │  ├─ 若不存在，在 LocalSet 中 spawn_local 新 Agent
   │  └─ 返回 AgentInstance
   ├─ 调用 agent.handle_chat(prompt)
   └─ 返回 GrpcChatResponse

4. handle_computer_chat()
   ├─ 更新会话映射: sessions[session_id] = SessionInfo
   └─ 返回 ChatResponse 给客户端

5. 客户端 → GET /computer/progress/{session_id}
   ├─ 通过 sessions 查找 SessionInfo（包含 ContainerKey）
   ├─ 建立 SSE 连接到容器
   └─ 实时推送进度事件
```

#### 3.3.2 VNC 桌面访问流程

```
1. 客户端 → GET /computer/desktop/{user_id}/{project_id}

2. computer_desktop_vnc()
   ├─ 查找 containers[ContainerKey::User(user_id)]
   ├─ 获取 container_ip
   └─ 构建目标 URL: ws://{container_ip}:6080

3. Pingora WebSocket 代理 (或 Nginx)
   ├─ 接收客户端 HTTP Upgrade 请求
   ├─ 升级为 WebSocket 连接
   ├─ 透明转发到容器的 6080 端口
   └─ 双向传输 WebSocket 帧

4. 容器内 noVNC (6080)
   ├─ 接收 WebSocket 连接
   ├─ 转发到 x11vnc (5900)
   └─ 返回桌面画面给客户端

5. 客户端浏览器显示远程桌面
```

#### 3.3.3 Agent 停止流程

```
1. 客户端 → POST /computer/agent/stop
   {
     "user_id": "user_123",
     "project_id": "proj_456"
   }

2. computer_agent_stop()
   ├─ 查找 containers[ContainerKey::User(user_id)]
   ├─ 从 container_info.projects 移除 project_id
   ├─ gRPC StopAgent RPC → agent_runner
   │  └─ ProjectAgentManager.stop_agent(project_id)
   │     ├─ 从 agents: DashMap 移除
   │     ├─ 取消 Agent 的 spawn_local 任务
   │     └─ 清理 Agent 资源
   ├─ 清理会话映射
   └─ 返回成功响应

注意：容器不会被销毁，继续运行其他 project_id
```

---

## 四、核心模块设计

### 4.1 数据模型

**设计目标**：使用统一的数据结构管理 RCoder 和 ComputerAgentRunner 两种模式的容器，避免维护多套独立的映射和结构。

**文件位置**: `crates/shared_types/src/model/computer_agent_model.rs`

#### 4.1.1 统一容器标识符（ContainerKey）

**设计目标**：使用统一的标识符来区分不同模式的容器，避免维护多套独立的映射。

```rust
/// 统一的容器标识符
/// 用于区分 RCoder 和 ComputerAgentRunner 两种模式的容器
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContainerKey {
    /// RCoder 模式：一个 project_id 对应一个容器
    Project(String),

    /// ComputerAgentRunner 模式：一个 user_id 对应一个容器
    User(String),
}

impl ContainerKey {
    /// 获取容器标识符的字符串形式（用于 Docker 容器查询）
    pub fn as_str(&self) -> &str {
        match self {
            ContainerKey::Project(id) => id,
            ContainerKey::User(id) => id,
        }
    }

    /// 获取 ServiceType
    pub fn service_type(&self) -> ServiceType {
        match self {
            ContainerKey::Project(_) => ServiceType::RCoder,
            ContainerKey::User(_) => ServiceType::ComputerAgentRunner,
        }
    }

    /// 从 project_id 创建
    pub fn from_project(project_id: String) -> Self {
        ContainerKey::Project(project_id)
    }

    /// 从 user_id 创建
    pub fn from_user(user_id: String) -> Self {
        ContainerKey::User(user_id)
    }
}

impl std::fmt::Display for ContainerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerKey::Project(id) => write!(f, "project:{}", id),
            ContainerKey::User(id) => write!(f, "user:{}", id),
        }
    }
}
```

#### 4.1.2 统一容器信息结构（UnifiedContainerInfo）

**设计目标**：合并 `UserContainerInfo` 和 `ProjectAndContainerInfo`，使用一个统一的结构来表示所有容器信息。

```rust
/// 统一的容器信息结构
/// 同时支持 RCoder 和 ComputerAgentRunner 两种模式
#[derive(Debug, Clone)]
pub struct UnifiedContainerInfo {
    /// 容器标识符（区分模式）
    pub key: ContainerKey,

    /// 容器基本信息
    pub container: ContainerBasicInfo,

    /// 服务类型
    pub service_type: ServiceType,

    /// 容器创建时间
    pub created_at: DateTime<Utc>,

    /// 最后活动时间（容器级别）
    pub last_activity: DateTime<Utc>,

    // ========== RCoder 模式字段 ==========

    /// RCoder 模式：当前会话 ID
    pub session_id: Option<String>,

    /// RCoder 模式：Agent 状态
    pub status: Option<AgentStatus>,

    /// RCoder 模式：模型配置
    pub model_provider: Option<ModelProviderConfig>,

    // ========== ComputerAgentRunner 模式字段 ==========

    /// ComputerAgentRunner 模式：容器内的所有项目映射
    /// key: project_id, value: ProjectInfo
    pub projects: Option<Arc<DashMap<String, Arc<ProjectInfo>>>>,
}

impl UnifiedContainerInfo {
    /// 创建 RCoder 模式的容器信息
    pub fn new_rcoder(project_id: String, container: ContainerBasicInfo) -> Self {
        let now = Utc::now();
        Self {
            key: ContainerKey::Project(project_id),
            container,
            service_type: ServiceType::RCoder,
            created_at: now,
            last_activity: now,
            session_id: None,
            status: None,
            model_provider: None,
            projects: None,
        }
    }

    /// 创建 ComputerAgentRunner 模式的容器信息
    pub fn new_computer(user_id: String, container: ContainerBasicInfo) -> Self {
        let now = Utc::now();
        Self {
            key: ContainerKey::User(user_id),
            container,
            service_type: ServiceType::ComputerAgentRunner,
            created_at: now,
            last_activity: now,
            session_id: None,
            status: None,
            model_provider: None,
            projects: Some(Arc::new(DashMap::new())),
        }
    }

    /// 更新活动时间
    pub fn update_activity(&mut self) {
        self.last_activity = Utc::now();
    }

    /// 获取或创建 projects 映射（仅 ComputerAgentRunner 模式）
    fn ensure_projects(&self) -> Arc<DashMap<String, Arc<ProjectInfo>>> {
        self.projects.clone().unwrap_or_else(|| Arc::new(DashMap::new()))
    }

    // ========== ComputerAgentRunner 专用方法 ==========

    /// 添加或更新项目（仅 ComputerAgentRunner 模式）
    pub fn upsert_project(&self, project_id: String, project_info: Arc<ProjectInfo>) {
        if let Some(projects) = &self.projects {
            projects.insert(project_id, project_info);
        }
    }

    /// 获取项目（仅 ComputerAgentRunner 模式）
    pub fn get_project(&self, project_id: &str) -> Option<Arc<ProjectInfo>> {
        self.projects.as_ref()?.get(project_id).map(|r| r.clone())
    }

    /// 移除项目（仅 ComputerAgentRunner 模式）
    pub fn remove_project(&self, project_id: &str) -> Option<Arc<ProjectInfo>> {
        self.projects.as_ref()?.remove(project_id).map(|(_, v)| v)
    }

    /// 列出所有项目 ID（仅 ComputerAgentRunner 模式）
    pub fn list_projects(&self) -> Vec<String> {
        self.projects
            .as_ref()
            .map(|p| p.iter().map(|r| r.key().clone()).collect())
            .unwrap_or_default()
    }

    /// 检查容器是否完全闲置
    /// - RCoder 模式：检查 status 是否为 Idle 且超时
    /// - ComputerAgentRunner 模式：检查所有项目是否都闲置且超时
    pub fn is_fully_idle(&self, idle_timeout: Duration) -> bool {
        let now = Utc::now();
        let idle_duration = now - self.last_activity;
        let is_timeout = idle_duration > chrono::Duration::from_std(idle_timeout).unwrap_or_default();

        match self.service_type {
            ServiceType::RCoder => {
                // RCoder 模式：检查自身状态
                let is_idle_status = matches!(self.status, Some(AgentStatus::Idle) | None);
                is_idle_status && is_timeout
            }
            ServiceType::ComputerAgentRunner => {
                // ComputerAgentRunner 模式：检查所有项目
                if let Some(projects) = &self.projects {
                    if projects.is_empty() {
                        return true; // 没有项目，可以清理
                    }

                    // 所有项目都必须闲置
                    projects.iter().all(|entry| {
                        let project_info = entry.value();
                        let project_idle_duration = now - project_info.last_activity;
                        let project_is_timeout = project_idle_duration >
                            chrono::Duration::from_std(idle_timeout).unwrap_or_default();

                        let is_idle_status = matches!(
                            project_info.status,
                            Some(AgentStatus::Idle) | None
                        );

                        is_idle_status && project_is_timeout
                    })
                } else {
                    true
                }
            }
        }
    }

    /// 获取容器 IP
    pub fn container_ip(&self) -> &str {
        &self.container.container_ip
    }
}

/// 项目信息（用于 ComputerAgentRunner 模式）
/// 简化版的项目元数据，不包含容器信息
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub project_id: String,
    pub session_id: Option<String>,
    pub status: Option<AgentStatus>,
    pub model_provider: Option<ModelProviderConfig>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}

impl ProjectInfo {
    pub fn new(project_id: String) -> Self {
        let now = Utc::now();
        Self {
            project_id,
            session_id: None,
            status: None,
            model_provider: None,
            created_at: now,
            last_activity: now,
        }
    }

    pub fn update_activity(&mut self) {
        self.last_activity = Utc::now();
    }

    pub fn update_session(&mut self, session_id: String) {
        self.session_id = Some(session_id);
        self.update_activity();
    }

    pub fn update_status(&mut self, status: AgentStatus) {
        self.status = Some(status);
        self.update_activity();
    }
}
```

#### 4.1.3 会话信息结构（SessionInfo）

```rust
/// 会话信息
/// 统一管理 RCoder 和 ComputerAgentRunner 的会话
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub container_key: ContainerKey,
    pub project_id: String,  // ComputerAgentRunner 模式下的 project_id
    pub created_at: DateTime<Utc>,
}

impl SessionInfo {
    pub fn new(session_id: String, container_key: ContainerKey, project_id: String) -> Self {
        Self {
            session_id,
            container_key,
            project_id,
            created_at: Utc::now(),
        }
    }
}
```

#### 4.1.4 简化后的 AppState

**文件**: `crates/rcoder/src/router.rs`

```rust
/// 应用状态（统一架构）
#[derive(Clone)]
pub struct AppState {
    /// 应用配置
    pub config: AppConfig,

    // ========== 核心映射（统一管理） ==========

    /// 统一的容器映射
    /// key: ContainerKey (Project/User), value: UnifiedContainerInfo
    ///
    /// 示例：
    /// - ContainerKey::Project("proj_123") -> RCoder 容器
    /// - ContainerKey::User("user_456") -> ComputerAgentRunner 容器
    pub containers: DashMap<ContainerKey, Arc<UnifiedContainerInfo>>,

    /// 会话映射
    /// key: session_id, value: SessionInfo (包含 ContainerKey 和 project_id)
    ///
    /// 用途：
    /// - 通过 session_id 快速查找对应的容器和项目
    /// - SSE 进度流使用此映射定位容器
    pub sessions: DashMap<String, Arc<SessionInfo>>,

    // ========== 索引映射（加速查询） ==========

    /// 项目到容器的索引
    /// key: project_id, value: ContainerKey
    ///
    /// 用途：
    /// - ComputerAgentRunner 模式：通过 project_id 查找所属的 user 容器
    /// - RCoder 模式：直接映射到 ContainerKey::Project
    pub project_to_container: DashMap<String, ContainerKey>,

    // ========== 共享组件 ==========

    /// Pingora 代理服务引用（用于读取指标）
    pub pingora_service: Option<Arc<pingora_proxy::PingoraProxyService>>,

    /// gRPC 连接池（与 agent_runner 通信）
    pub grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
}

impl AppState {
    pub fn new(config: AppConfig, grpc_pool: Arc<crate::grpc::GrpcChannelPool>) -> Self {
        Self {
            config,
            containers: DashMap::new(),
            sessions: DashMap::new(),
            project_to_container: DashMap::new(),
            pingora_service: None,
            grpc_pool,
        }
    }

    // ========== 便捷方法 ==========

    /// 获取容器信息（通过 ContainerKey）
    pub fn get_container(&self, key: &ContainerKey) -> Option<Arc<UnifiedContainerInfo>> {
        self.containers.get(key).map(|r| r.clone())
    }

    /// 获取容器信息（通过 project_id）
    pub fn get_container_by_project(&self, project_id: &str) -> Option<Arc<UnifiedContainerInfo>> {
        let key = self.project_to_container.get(project_id)?;
        self.containers.get(key.value()).map(|r| r.clone())
    }

    /// 获取会话信息
    pub fn get_session(&self, session_id: &str) -> Option<Arc<SessionInfo>> {
        self.sessions.get(session_id).map(|r| r.clone())
    }

    /// 添加或更新容器
    pub fn upsert_container(&self, key: ContainerKey, info: Arc<UnifiedContainerInfo>) {
        self.containers.insert(key, info);
    }

    /// 添加会话
    pub fn add_session(&self, session_id: String, session_info: Arc<SessionInfo>) {
        self.sessions.insert(session_id, session_info);
    }

    /// 移除容器（包括清理所有相关映射）
    pub fn remove_container(&self, key: &ContainerKey) -> Option<Arc<UnifiedContainerInfo>> {
        // 移除容器
        let container = self.containers.remove(key).map(|(_, v)| v)?;

        // 清理 project_to_container 索引
        match key {
            ContainerKey::Project(project_id) => {
                self.project_to_container.remove(project_id);
            }
            ContainerKey::User(_) => {
                // ComputerAgentRunner 模式：清理所有项目的索引
                if let Some(projects) = &container.projects {
                    for entry in projects.iter() {
                        self.project_to_container.remove(entry.key());
                    }
                }
            }
        }

        // 清理相关会话
        let sessions_to_remove: Vec<String> = self.sessions
            .iter()
            .filter(|entry| &entry.value().container_key == key)
            .map(|entry| entry.key().clone())
            .collect();

        for session_id in sessions_to_remove {
            self.sessions.remove(&session_id);
        }

        Some(container)
    }
}
```

#### 4.1.5 对比：简化前后的映射数量

| 项目 | 简化前 | 简化后 | 减少 |
|------|-------|-------|------|
| 核心映射 | 6 个 DashMap | 3 个 DashMap | **-50%** |
| 数据结构 | 2 个独立结构 | 1 个统一结构 | **统一** |
| 维护成本 | 高（独立逻辑） | 低（共享逻辑） | **显著降低** |

**简化收益**：
1. ✅ **统一标识符**：`ContainerKey` 统一管理不同模式的容器
2. ✅ **统一数据结构**：`UnifiedContainerInfo` 合并两种模式的信息
3. ✅ **减少映射数量**：从 6 个减少到 3 个
4. ✅ **简化查询逻辑**：通过 `ContainerKey` 直接查询，无需判断模式
5. ✅ **便于扩展**：未来添加新模式只需扩展 `ContainerKey` 枚举
6. ✅ **降低维护成本**：清理、查询、更新逻辑统一处理

### 4.2 容器管理模块

#### 4.2.1 ComputerContainerManager

**文件**: `crates/rcoder/src/service/computer_container_manager.rs`

```rust
use crate::error::AppError;
use docker_manager::{self, DockerContainerInfo, ContainerBasicInfo};
use shared_types::{ServiceType, ServiceResourceLimits};
use std::path::PathBuf;
use tracing::{info, error};

/// Computer Agent Runner 容器管理器
pub struct ComputerContainerManager;

impl ComputerContainerManager {
    /// 根据 user_id 获取或创建容器
    ///
    /// 容器命名: computer-agent-runner-{user_id}
    /// 工作区: /app/computer-project-workspace/{user_id}
    pub async fn get_or_create_container_for_user(
        user_id: &str,
        resource_limits: Option<ServiceResourceLimits>,
    ) -> Result<ContainerBasicInfo, AppError> {
        info!("🔍 [COMPUTER] 获取/创建用户容器: user_id={}", user_id);

        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| AppError::internal_server_error(
                &format!("获取 DockerManager 失败: {}", e)
            ))?;

        // 使用 user_id 作为 project_id 来查询容器
        // 因为 ComputerAgentRunner 容器是按 user_id 创建的
        if let Ok(Some(info)) = docker_manager.get_agent_info(user_id).await {
            info!("✅ [COMPUTER] 用户容器已存在: container_id={}", info.container_id);
            return Ok(info);
        }

        // 创建新容器
        info!("🏗️ [COMPUTER] 创建新用户容器: user_id={}", user_id);
        Self::create_container_for_user(user_id, &docker_manager, resource_limits).await
    }

    /// 为用户创建容器
    async fn create_container_for_user(
        user_id: &str,
        docker_manager: &std::sync::Arc<docker_manager::DockerManager>,
        resource_limits: Option<ServiceResourceLimits>,
    ) -> Result<ContainerBasicInfo, AppError> {
        // 1. 准备用户级工作目录
        let user_workspace = Self::get_user_workspace(user_id).await?;
        Self::create_user_workspace(user_id).await?;

        // 2. 解析宿主机路径
        let host_path = crate::utils::resolve_container_path_to_host(&user_workspace)
            .await
            .map_err(|e| AppError::internal_server_error(
                &format!("路径解析失败: {}", e)
            ))?;

        info!(
            "📁 [COMPUTER] 用户工作区路径映射: 容器内={:?}, 宿主机={:?}",
            user_workspace, host_path
        );

        // 3. 调用 DockerManager 启动容器
        // 注意: 使用 user_id 作为 project_id 传递给 Docker Manager
        let container_info = docker_manager
            .start_agent_container(
                user_id,  // 使用 user_id 作为容器标识
                &host_path.to_string_lossy(),
                ServiceType::ComputerAgentRunner,
                resource_limits,
            )
            .await
            .map_err(|e| AppError::internal_server_error(
                &format!("启动容器失败: {}", e)
            ))?;

        info!(
            "🚀 [COMPUTER] 用户容器创建成功: container_id={}",
            container_info.container_id
        );

        Ok(container_info)
    }

    /// 获取用户工作区路径
    ///
    /// 格式: /app/computer-project-workspace/{user_id}
    /// 注意：project_id 作为子目录由 agent 自己管理
    pub async fn get_user_workspace(user_id: &str) -> Result<PathBuf, AppError> {
        let workspace_dir = PathBuf::from("/app/computer-project-workspace");
        let user_dir = workspace_dir.join(user_id);
        Ok(user_dir)
    }

    /// 创建用户工作区目录
    async fn create_user_workspace(user_id: &str) -> Result<PathBuf, AppError> {
        let workspace_dir = PathBuf::from("/app/computer-project-workspace");

        // 确保根目录存在
        tokio::fs::create_dir_all(&workspace_dir).await
            .map_err(|e| AppError::internal_server_error(
                &format!("创建 workspace 目录失败: {}", e)
            ))?;

        // 创建用户目录
        let user_dir = workspace_dir.join(user_id);
        tokio::fs::create_dir_all(&user_dir).await
            .map_err(|e| AppError::internal_server_error(
                &format!("创建用户目录失败: {}", e)
            ))?;

        Ok(user_dir)
    }
}
```

### 4.3 HTTP 接口模块

#### 4.3.1 ComputerChatRequest 结构

**文件**: `crates/rcoder/src/handler/computer_chat_handler.rs`

```rust
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use shared_types::*;

/// Computer Agent 聊天请求
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct ComputerChatRequest {
    /// 用户 ID (必填) - 一个用户对应一个容器
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目 ID (可选) - 一个容器内可以有多个项目
    /// 若未提供，系统自动生成 UUID
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 用户输入的 prompt
    #[schema(example = "帮我创建一个 React 应用")]
    pub prompt: String,

    // ========== 以下字段与现有 ChatRequest 保持一致 ==========

    /// 会话 ID (可选) - 用于续传会话
    pub session_id: Option<String>,

    /// 多媒体附件
    #[serde(default)]
    pub attachments: Vec<Attachment>,

    /// 数据源附件
    #[serde(default)]
    pub data_source_attachments: Vec<String>,

    /// 模型配置
    pub model_provider: Option<ModelProviderConfig>,

    /// 请求 ID
    pub request_id: Option<String>,

    /// 系统提示词覆盖
    pub system_prompt: Option<String>,

    /// 用户提示词模板
    pub user_prompt: Option<String>,

    /// Agent 运行时配置
    pub agent_config: Option<ChatAgentConfig>,
}

/// Computer Agent 停止请求
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct ComputerAgentStopRequest {
    /// 用户 ID (必填)
    pub user_id: String,

    /// 项目 ID (必填) - 只停止特定项目的 agent
    pub project_id: String,

    /// 可选的会话 ID
    pub session_id: Option<String>,
}
```

#### 4.3.2 handler 实现（关键代码）

```rust
/// 处理 Computer Agent 聊天请求（使用统一架构）
pub async fn handle_computer_chat(
    State(state): State<Arc<AppState>>,
    Json(mut request): Json<ComputerChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    let user_id = request.user_id.clone();

    // 生成或使用提供的 project_id
    let project_id = match &request.project_id {
        Some(id) => id.clone(),
        None => {
            let project_id = generate_project_id();
            request.project_id = Some(project_id.clone());
            project_id
        }
    };

    info!(
        "🚀 [COMPUTER_CHAT] user_id={}, project_id={}, prompt_len={}",
        user_id, project_id, request.prompt.len()
    );

    // 步骤 1: 获取或创建用户容器
    let container_basic_info = ComputerContainerManager::get_or_create_container_for_user(
        &user_id,
        request.agent_config.as_ref().and_then(|c| c.resource_limits.clone()),
    ).await?;

    // 步骤 2: 获取或创建统一容器信息
    let container_key = ContainerKey::from_user(user_id.clone());
    let container_info = {
        let entry = state.containers.entry(container_key.clone());
        match entry {
            dashmap::mapref::entry::Entry::Occupied(occupied) => {
                occupied.get().clone()
            }
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                let new_info = Arc::new(UnifiedContainerInfo::new_computer(
                    user_id.clone(),
                    container_basic_info.clone(),
                ));
                vacant.insert(new_info.clone());
                new_info
            }
        }
    };

    // 步骤 3: 创建或更新项目信息
    let project_info = {
        if let Some(existing) = container_info.get_project(&project_id) {
            // 更新现有项目
            let mut updated = (**existing).clone();
            updated.update_activity();
            if let Some(model) = request.model_provider.clone() {
                updated.model_provider = Some(model);
            }
            Arc::new(updated)
        } else {
            // 创建新项目
            let mut new_project = ProjectInfo::new(project_id.clone());
            new_project.model_provider = request.model_provider.clone();
            Arc::new(new_project)
        }
    };

    // 将项目添加到容器
    container_info.upsert_project(project_id.clone(), project_info.clone());

    // 步骤 4: 建立项目到容器的索引
    state.project_to_container.insert(
        project_id.clone(),
        container_key.clone(),
    );

    // 步骤 5: 转发请求到容器 (仅使用 gRPC)
    let result = forward_computer_request_to_container(
        &request,
        &container_basic_info,
        &state.grpc_pool,
    ).await?;

    // 步骤 6: 更新会话映射
    if let Some(chat_response) = &result.data {
        let session_id = chat_response.session_id.clone();

        // 更新项目的 session_id
        if let Some(project) = container_info.get_project(&project_id) {
            let mut updated = (**project).clone();
            updated.update_session(session_id.clone());
            container_info.upsert_project(project_id.clone(), Arc::new(updated));
        }

        // 建立会话映射
        let session_info = Arc::new(SessionInfo::new(
            session_id.clone(),
            container_key,
            project_id.clone(),
        ));
        state.add_session(session_id.clone(), session_info);

        info!(
            "✅ [COMPUTER_CHAT] 会话建立: session_id={}, user_id={}, project_id={}",
            session_id, user_id, project_id
        );
    }

    Ok(result)
}
```

### 4.4 agent_runner 多实例支持

#### 4.4.1 ProjectAgentManager

**文件**: `crates/agent_runner/src/service/project_agent_manager.rs`

```rust
use dashmap::DashMap;
use std::sync::Arc;
use tracing::{info, warn, error};
use anyhow::Result;

/// Agent 实例
/// 封装单个 project_id 对应的 agent 运行时
pub struct AgentInstance {
    pub project_id: String,
    // TODO: 添加 ACP agent 相关字段
    // pub agent_handle: tokio::task::JoinHandle<()>,
    // pub sender: mpsc::UnboundedSender<AgentMessage>,
}

/// 项目 Agent 管理器
/// 在一个容器内管理多个 project_id 对应的 agent 实例
pub struct ProjectAgentManager {
    /// 所有 agent 实例映射
    /// key: project_id, value: AgentInstance
    agents: DashMap<String, Arc<AgentInstance>>,
}

impl ProjectAgentManager {
    pub fn new() -> Self {
        Self {
            agents: DashMap::new(),
        }
    }

    /// 获取或创建 agent 实例
    ///
    /// 注意：必须在 LocalSet 上下文中调用
    pub async fn get_or_create_agent(&self, project_id: &str) -> Result<Arc<AgentInstance>> {
        // 检查是否已存在
        if let Some(agent) = self.agents.get(project_id) {
            info!("✅ [AGENT_MGR] Agent 已存在: project_id={}", project_id);
            return Ok(agent.clone());
        }

        // 创建新的 agent 实例
        info!("🏗️ [AGENT_MGR] 创建新 Agent: project_id={}", project_id);

        // TODO: 实现 agent 创建逻辑
        // 1. 在 LocalSet 中 spawn_local 新任务
        // 2. 初始化 ACP 连接
        // 3. 配置 MCP 工具（Chrome DevTools）

        let agent = Arc::new(AgentInstance {
            project_id: project_id.to_string(),
        });

        // 添加到映射
        self.agents.insert(project_id.to_string(), agent.clone());

        Ok(agent)
    }

    /// 停止特定 project_id 的 agent
    pub async fn stop_agent(&self, project_id: &str) -> Result<()> {
        info!("🛑 [AGENT_MGR] 停止 Agent: project_id={}", project_id);

        if let Some((_, agent)) = self.agents.remove(project_id) {
            // TODO: 实现 agent 停止逻辑
            // 1. 取消 spawn_local 任务
            // 2. 关闭 ACP 连接
            // 3. 清理资源

            info!("✅ [AGENT_MGR] Agent 已停止: project_id={}", project_id);
            Ok(())
        } else {
            warn!("⚠️ [AGENT_MGR] Agent 不存在: project_id={}", project_id);
            Err(anyhow::anyhow!("Agent 不存在: {}", project_id))
        }
    }

    /// 列出所有 agent
    pub fn list_agents(&self) -> Vec<String> {
        self.agents.iter().map(|r| r.key().clone()).collect()
    }

    /// 获取 agent 数量
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }
}
```

#### 4.4.2 gRPC Proto 扩展

**文件**: `crates/shared_types/proto/agent.proto`

```protobuf
service AgentService {
    // 现有方法
    rpc Chat(GrpcChatRequest) returns (GrpcChatResponse);
    rpc SubscribeProgress(ProgressRequest) returns (stream ProgressEvent);
    rpc CancelSession(CancelRequest) returns (CancelResponse);
    rpc GetStatus(StatusRequest) returns (StatusResponse);

    // 新增：停止特定 project_id 的 agent
    rpc StopAgent(StopAgentRequest) returns (StopAgentResponse);
}

message StopAgentRequest {
    string project_id = 1;
}

message StopAgentResponse {
    bool success = 1;
    string message = 2;
    string project_id = 3;
}
```

### 4.5 闲置检测和清理

#### 4.5.1 统一的清理逻辑

**文件**: `crates/rcoder/src/proxy_agent/cleanup_task.rs`

```rust
impl AgentCleaner {
    /// 清理闲置 agent（统一处理 RCoder 和 ComputerAgentRunner）
    async fn cleanup_idle_agents(&mut self) -> Result<CleanupStats> {
        let mut stats = CleanupStats::default();
        let current_time = Utc::now();

        // 收集需要清理的容器
        let mut containers_to_clean = Vec::new();

        for entry in self.state.containers.iter() {
            let container_key = entry.key();
            let container_info = entry.value();

            // 检查容器保护期（创建后 5 分钟内不清理）
            let protection_time = chrono::Duration::minutes(5);
            if current_time - container_info.created_at < protection_time {
                info!(
                    "🛡️ [CLEANUP] 容器处于保护期: key={}",
                    container_key
                );
                continue;
            }

            // 使用统一的闲置判断方法
            if container_info.is_fully_idle(self.config.idle_timeout) {
                info!(
                    "🎯 [CLEANUP] 发现闲置容器: key={}, service_type={:?}",
                    container_key,
                    container_info.service_type
                );

                // 记录额外信息
                match &container_info.service_type {
                    ServiceType::RCoder => {
                        info!("   - RCoder 模式，单项目容器");
                    }
                    ServiceType::ComputerAgentRunner => {
                        let project_count = container_info.list_projects().len();
                        info!("   - ComputerAgentRunner 模式，包含 {} 个项目", project_count);
                    }
                }

                containers_to_clean.push(container_key.clone());
            }
        }

        // 执行清理
        for container_key in containers_to_clean {
            match self.cleanup_container(&container_key).await {
                Ok(_) => {
                    stats.cleaned_count += 1;
                    info!("✅ [CLEANUP] 成功清理容器: {}", container_key);
                }
                Err(e) => {
                    stats.failed_count += 1;
                    warn!("❌ [CLEANUP] 清理失败: {} - {}", container_key, e);
                }
            }
        }

        Ok(stats)
    }

    /// 清理单个容器（统一处理）
    async fn cleanup_container(&self, container_key: &ContainerKey) -> Result<()> {
        info!("🔥 [CLEANUP] 开始清理容器: {}", container_key);

        // 1. 获取容器信息
        let container_info = self.state.containers.get(container_key)
            .ok_or_else(|| anyhow::anyhow!("容器不存在"))?
            .clone();

        // 2. 获取 Docker Manager
        let docker_manager = docker_manager::global::get_global_docker_manager().await?;

        // 3. 销毁 Docker 容器
        // 注意：使用 container_key.as_str() 作为标识符查询
        if let Ok(Some(container_basic_info)) = docker_manager.get_agent_info(container_key.as_str()).await {
            // 清理 gRPC 连接池
            let grpc_addr = format!("{}:{}",
                container_basic_info.container_ip,
                shared_types::GRPC_DEFAULT_PORT
            );
            self.state.grpc_pool.remove(&grpc_addr);

            // 执行物理销毁
            docker_manager.stop_container_by_id(&container_basic_info.container_id).await?;

            info!(
                "✅ [CLEANUP] Docker 容器已销毁: container_id={}",
                container_basic_info.container_id
            );
        }

        // 4. 使用 AppState 的统一清理方法
        self.state.remove_container(container_key);

        info!("✅ [CLEANUP] 容器清理完成: {}", container_key);
        Ok(())
    }

    /// 孤立容器检测（统一处理）
    async fn cleanup_orphaned_containers(&mut self) -> u64 {
        let mut cleaned_count = 0;
        let docker_manager = match docker_manager::global::get_global_docker_manager().await {
            Ok(dm) => dm,
            Err(e) => {
                error!("❌ [CLEANUP] 获取 DockerManager 失败: {}", e);
                return 0;
            }
        };

        // 收集所有应该存在的容器 ID
        let expected_containers: HashSet<String> = self.state.containers
            .iter()
            .map(|entry| entry.value().container.container_id.clone())
            .collect();

        // 列出所有 Docker 容器（RCoder 和 ComputerAgentRunner）
        let patterns = vec![
            "rcoder-agent-",
            "computer-agent-runner-",
        ];

        for pattern in patterns {
            if let Ok(containers) = docker_manager.list_containers_by_pattern(pattern).await {
                for container in containers {
                    if !expected_containers.contains(&container.id) {
                        info!(
                            "🎯 [CLEANUP] 发现孤立容器: id={}, name={}",
                            container.id, container.names.join(",")
                        );

                        // 清理孤立容器
                        if let Err(e) = docker_manager.stop_container_by_id(&container.id).await {
                            warn!("❌ [CLEANUP] 清理孤立容器失败: {} - {}", container.id, e);
                        } else {
                            cleaned_count += 1;
                            info!("✅ [CLEANUP] 孤立容器已清理: {}", container.id);
                        }
                    }
                }
            }
        }

        cleaned_count
    }
}

/// 清理统计
#[derive(Debug, Default)]
pub struct CleanupStats {
    pub cleaned_count: u64,
    pub failed_count: u64,
}
```

**关键改进**：

1. ✅ **统一清理逻辑**：不再区分 RCoder 和 ComputerAgentRunner 的清理流程
2. ✅ **使用 `is_fully_idle()`**：统一的闲置判断方法，自动根据 `ServiceType` 处理
3. ✅ **使用 `remove_container()`**：AppState 的统一清理方法，自动处理所有相关映射
4. ✅ **孤立容器检测**：支持检测两种模式的孤立容器
5. ✅ **简化代码**：从原来的独立清理方法合并为一个统一方法

---

## 五、接口设计

### 5.1 HTTP API

| 端点 | 方法 | 请求体 | 响应体 | 说明 |
|------|------|--------|--------|------|
| `/computer/chat` | POST | `ComputerChatRequest` | `HttpResult<ChatResponse>` | 发送聊天请求 |
| `/computer/agent/stop` | POST | `ComputerAgentStopRequest` | `HttpResult<StopAgentResponse>` | 停止 Agent |
| `/computer/progress/{session_id}` | GET | - | SSE Stream | 实时进度流 |
| `/computer/desktop/{user_id}/{project_id}` | GET | - | WebSocket Upgrade | VNC 桌面访问 |

### 5.2 gRPC API

| RPC 方法 | 请求 | 响应 | 说明 |
|---------|------|------|------|
| `Chat` | `GrpcChatRequest` | `GrpcChatResponse` | 聊天请求（已扩展支持 project_id 路由） |
| `StopAgent` | `StopAgentRequest` | `StopAgentResponse` | 停止特定 project_id 的 agent |
| `SubscribeProgress` | `ProgressRequest` | Stream `ProgressEvent` | 订阅进度事件 |

---

## 六、部署配置

### 6.1 Docker Compose 配置

**文件**: `docker/docker-compose.yml`（确认挂载）

```yaml
services:
  rcoder:
    image: ${RCODER_IMAGE:-master-rcoder:latest}
    container_name: rcoder
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - ./project_workspace:/app/project_workspace  # RCoder 工作区
      - ./computer-project-workspace:/app/computer-project-workspace  # Computer Agent 工作区
      - ./logs:/app/logs
    ports:
      - "8087:8087"
    environment:
      - RUST_LOG=info
      - ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY}
```

### 6.2 Agent 配置文件

**文件**: `crates/agent_config/configs/computer_agent_default.json`

```json
{
  "agent_servers": {
    "claude-code-acp": {
      "agent_id": "claude-code-acp",
      "agent_type": "claude",
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
        "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}",
        "ANTHROPIC_BASE_URL": "{MODEL_PROVIDER_BASE_URL}",
        "RUST_LOG": "info"
      },
      "system_prompt": {
        "source": "embedded",
        "template": "",
        "enabled": true
      },
      "user_prompt": {
        "template": "{user_prompt}",
        "enabled": false
      },
      "installation": {
        "package_manager": "npm",
        "package_name": "@anthropics/claude-code-acp",
        "version": "latest"
      },
      "enabled": true,
      "metadata": {
        "description": "Claude Code ACP Agent - Computer Agent Runner 配置",
        "version": "1.0.0"
      }
    }
  },
  "context_servers": {
    "chrome-devtools": {
      "source": "custom",
      "enabled": true,
      "command": "npx",
      "args": ["-y", "chrome-devtools-mcp@latest"],
      "env": {
        "CHROME_REMOTE_DEBUGGING_PORT": "9222"
      },
      "metadata": {
        "description": "Chrome DevTools MCP - 浏览器操作能力",
        "documentation": "https://github.com/ChromeDevTools/chrome-devtools-mcp"
      }
    },
    "context7": {
      "source": "custom",
      "enabled": true,
      "command": "bunx",
      "args": ["-y", "@upstash/context7-mcp"],
      "env": {}
    },
    "fetch": {
      "source": "custom",
      "enabled": true,
      "command": "uvx",
      "args": ["mcp-server-fetch"],
      "env": {}
    }
  }
}
```

---

## 七、实施计划

### 7.1 阶段划分

| 阶段 | 工作内容 | 工作量 | 优先级 | 依赖 |
|------|---------|--------|--------|------|
| 阶段 1 | 核心数据结构 | 1-2 天 | P0 | 无 |
| 阶段 2 | HTTP 接口实现 | 1 天 | P0 | 阶段 1 |
| 阶段 3 | agent_runner 多实例支持 | 2-3 天 | P0 | 阶段 1 |
| 阶段 4 | 闲置检测和清理 | 1 天 | P1 | 阶段 1, 2 |
| 阶段 5 | VNC 代理实现 | 1-2 天 | P1 | 阶段 2 |
| 阶段 6 | MCP 配置和优化 | 0.5 天 | P2 | 阶段 3 |
| **总计** | | **6.5-9.5 天** | | |

### 7.2 关键里程碑

- ✅ **M1**: 完成核心数据结构定义（阶段 1）
- ✅ **M2**: HTTP `/computer/chat` 接口可用（阶段 2）
- ✅ **M3**: agent_runner 支持多 Agent 实例（阶段 3，关键里程碑）
- ✅ **M4**: 闲置检测和清理正常运行（阶段 4）
- ✅ **M5**: VNC 桌面可访问（阶段 5）
- ✅ **M6**: 完整功能上线（阶段 6）

### 7.3 验收标准

#### 功能验收
- [ ] 可以通过 `POST /computer/chat` 发送请求，自动创建 user_id 对应的容器
- [ ] 同一 user_id 的多个 project_id 可以在同一容器内运行
- [ ] 可以通过 `GET /computer/desktop/{user_id}/{project_id}` 访问 VNC 桌面
- [ ] Agent 可以通过 Chrome DevTools MCP 操作 Chromium 浏览器
- [ ] 可以通过 `POST /computer/agent/stop` 停止单个 project_id 的 agent（不销毁容器）
- [ ] 只有当 user_id 下所有 project_id 都闲置时才销毁容器

#### 性能验收
- [ ] 单个容器可以稳定运行 3+ 个 project_id 的 agent
- [ ] VNC 桌面访问延迟 < 500ms
- [ ] 容器创建时间 < 30s

#### 安全验收
- [ ] user_id 只能访问自己的容器和 VNC
- [ ] project_id 之间没有上下文污染
- [ ] 容器资源限额生效

---

## 八、风险和挑战

### 8.1 技术风险

| 风险 | 影响 | 概率 | 缓解措施 | 责任人 |
|------|------|------|----------|--------|
| agent_runner LocalSet 并发管理复杂 | 高 | 高 | 参考现有 ACP agent worker 模式，使用 `spawn_local` 隔离 | 开发 |
| Pingora 不支持 WebSocket | 中 | 中 | 备用方案：使用 Nginx 作为 VNC 专用代理 | 开发 |
| 容器资源耗尽 | 高 | 中 | 实现严格的资源限额和监控 | 运维 |
| VNC 安全风险 | 高 | 低 | 实现一次性 token 和会话超时（后续阶段） | 安全 |
| 多 agent 内存泄漏 | 中 | 中 | 实现 agent 生命周期管理和定期清理 | 开发 |

### 8.2 业务风险

| 风险 | 影响 | 概率 | 缓解措施 |
|------|------|------|----------|
| 用户需求理解偏差 | 高 | 低 | 原型演示，快速迭代 |
| 与现有功能冲突 | 中 | 中 | 充分的兼容性测试 |
| 容器成本过高 | 中 | 中 | 实施严格的闲置清理策略 |

---

## 九、后续规划

### 9.1 第一阶段优化（MVP 后）

- 实现 VNC 会话录制和回放
- 增加剪贴板 API 支持
- 增强 Agent 监控和日志

### 9.2 第二阶段扩展

- 支持多用户协作（共享 VNC 桌面）
- 集成更多 MCP 工具（文件操作、数据库等）
- 实现 Agent 性能分析和优化

### 9.3 长期愿景

- 构建 Agent 市场，用户可以共享和复用 Agent 配置
- 支持自定义桌面环境和应用
- 实现 Agent 集群调度和负载均衡

---

## 十、附录

### 10.1 术语表

| 术语 | 说明 |
|------|------|
| user_id | 用户唯一标识符 |
| project_id | 项目唯一标识符 |
| ServiceType | 服务类型枚举（RCoder, ComputerAgentRunner） |
| ACP | Agent Client Protocol，AI Agent 通信协议 |
| MCP | Model Context Protocol，模型上下文协议 |
| CDP | Chrome DevTools Protocol，浏览器调试协议 |
| noVNC | 基于 HTML5 的 VNC 客户端 |
| LocalSet | Tokio 的本地任务集，支持 !Send 任务 |
| DashMap | 高性能的并发 HashMap |

### 10.2 参考资料

1. **rcoder 项目文档**:
   - `CLAUDE.md` - 项目概述
   - `crates/agent_runner/README.md` - Agent Runner 文档

2. **外部依赖文档**:
   - [ACP Protocol](https://github.com/anthropics/agent-client-protocol)
   - [Chrome DevTools MCP](https://github.com/ChromeDevTools/chrome-devtools-mcp)
   - [noVNC Documentation](https://github.com/novnc/noVNC)

3. **技术博客**:
   - Rust Tokio LocalSet 使用指南
   - WebSocket 代理实现最佳实践

---

**文档变更历史**

| 版本 | 日期 | 作者 | 变更说明 |
|------|------|------|----------|
| v1.0 | 2025-12-10 | Claude | 初始版本 |

---

**审批记录**

| 角色 | 姓名 | 审批意见 | 日期 |
|------|------|----------|------|
| 产品负责人 | | | |
| 技术负责人 | | | |
| 安全负责人 | | | |
