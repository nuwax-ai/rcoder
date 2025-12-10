# Computer Agent Runner 详细实施计划

**文档版本**: v1.0
**创建日期**: 2025-12-10
**作者**: Claude (基于需求设计文档)
**项目**: rcoder - AI 驱动开发平台
**基准文档**: `specs/computer-agent-runner/0001-spec-claude.md`

---

## 一、项目概述与目标

### 1.1 功能简介

Computer Agent Runner 是 rcoder 项目的扩展功能模块，旨在为 AI Agent 提供完整的虚拟桌面环境，使其能够：

- **操作浏览器**: 在虚拟桌面中打开 Chromium，自主搜索和访问网络资料
- **远程监控**: 用户可通过 VNC 远程查看 Agent 的操作过程
- **复杂任务处理**: Agent 在容器内完成复杂的多步骤任务（如网页抓取、数据处理等）
- **资源共享**: 一个用户可以有多个项目，共享同一个桌面环境容器

### 1.2 核心特点

| 特点 | 说明 |
|------|------|
| **用户级容器** | 一个 `user_id` 对应一个带桌面环境的容器 |
| **多 Agent 实例** | 容器内可同时运行多个 `project_id` 对应的 AI Agent 实例 |
| **统一架构** | 使用 `ContainerKey` 和 `UnifiedContainerInfo` 统一管理两种模式 |
| **VNC 访问** | 提供 VNC 远程桌面访问，用户可实时查看 Agent 操作 |
| **浏览器操作** | 集成 Chrome DevTools MCP，赋予 Agent 浏览器操作能力 |
| **智能清理** | 只有当用户下所有项目都闲置时才销毁容器 |

### 1.3 与现有 RCoder 的对比

| 维度 | RCoder (现有) | Computer Agent Runner (新) |
|------|--------------|------------------------------|
| 容器标识 | `project_id` | `user_id` |
| 容器命名 | `rcoder-agent-{project_id}` | `computer-agent-runner-{user_id}` |
| Agent 实例数 | 1 个 | 多个（按 `project_id` 区分） |
| 工作目录 | `/app/project_workspace/{project_id}` | `/app/computer-project-workspace/{user_id}` |
| 桌面环境 | 无 | XFCE4 + noVNC |
| 浏览器 | 无 | Chromium + CDP |
| 闲置策略 | project_id 闲置即销毁 | user_id 下所有 project_id 都闲置才销毁 |

---

## 二、实施准备

### 2.1 环境准备

#### 开发环境配置
- **Rust**: 1.75+ (2024 Edition)
- **Docker**: 支持 BuildKit 和多架构构建
- **Docker Compose**: v2.0+
- **开发工具**: cargo, rustfmt, clippy

#### Docker 镜像准备
- 确认 `docker/rcoder-agent-runner/Dockerfile` 包含：
  - XFCE4 桌面环境
  - noVNC 服务（端口 6080）
  - Chromium 浏览器（CDP 端口 9222）
  - 所有必要的系统依赖

#### 依赖库版本确认
```toml
[dependencies]
tokio = { version = "1.35", features = ["full"] }
tonic = "0.14.2"
agent-client-protocol = "0.6"
dashmap = "5.5"
chrono = { version = "0.4", features = ["serde"] }
```

### 2.2 技术栈确认

| 技术 | 版本/组件 | 用途 |
|------|----------|------|
| Rust | 1.75+ | 核心开发语言 |
| gRPC | Tonic 0.14.2 | 内部通信协议 |
| ACP | 0.6 / 0.4 | Agent 客户端协议 |
| Docker | 最新稳定版 | 容器化部署 |
| VNC/noVNC | 标准协议 | 远程桌面访问 |
| Chromium | Debian 12 stable | 浏览器环境 |
| XFCE4 | Debian 12 stable | 桌面环境 |

---

## 三、详细实施步骤

### 阶段 1：核心数据结构实现（1-2天，P0）

#### 目标
实现统一架构的核心数据模型，使用 `ContainerKey` 枚举和 `UnifiedContainerInfo` 结构统一管理 RCoder 和 ComputerAgentRunner 两种模式的容器。

#### 步骤 1.1：创建 ContainerKey 枚举

**文件**: `crates/shared_types/src/model/computer_agent_model.rs`

**任务清单**:
- [ ] 定义 `ContainerKey` 枚举，包含 `Project(String)` 和 `User(String)` 两种变体
- [ ] 实现 `as_str()` 方法：获取容器标识符的字符串形式
- [ ] 实现 `service_type()` 方法：返回对应的 `ServiceType`
- [ ] 实现 `from_project()` 和 `from_user()` 构造方法
- [ ] 实现 `Display` trait，格式为 `"project:{id}"` 或 `"user:{id}"`
- [ ] 实现 `Hash`, `Eq`, `PartialEq` trait（用于 DashMap）
- [ ] 实现 `Serialize`, `Deserialize` trait（用于持久化）

**关键代码模式**:
```rust
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContainerKey {
    /// RCoder 模式：一个 project_id 对应一个容器
    Project(String),

    /// ComputerAgentRunner 模式：一个 user_id 对应一个容器
    User(String),
}

impl ContainerKey {
    pub fn as_str(&self) -> &str {
        match self {
            ContainerKey::Project(id) => id,
            ContainerKey::User(id) => id,
        }
    }

    pub fn service_type(&self) -> ServiceType {
        match self {
            ContainerKey::Project(_) => ServiceType::RCoder,
            ContainerKey::User(_) => ServiceType::ComputerAgentRunner,
        }
    }
}
```

#### 步骤 1.2：实现 UnifiedContainerInfo 结构

**任务清单**:
- [ ] 定义 `UnifiedContainerInfo` 结构体，合并 RCoder 和 ComputerAgentRunner 两种模式
- [ ] 实现 `new_rcoder()` 构造方法（RCoder 模式）
- [ ] 实现 `new_computer()` 构造方法（ComputerAgentRunner 模式）
- [ ] 实现 `update_activity()` 方法：更新活动时间
- [ ] 实现 ComputerAgentRunner 专用方法：
  - [ ] `upsert_project()`: 添加或更新项目
  - [ ] `get_project()`: 获取项目
  - [ ] `remove_project()`: 移除项目
  - [ ] `list_projects()`: 列出所有项目 ID
- [ ] 实现 `is_fully_idle()` 方法：统一的闲置判断逻辑
- [ ] 实现 `container_ip()` 和 `container_id()` 便捷方法

**关键代码模式**:
```rust
#[derive(Debug, Clone)]
pub struct UnifiedContainerInfo {
    pub key: ContainerKey,
    pub container: ContainerBasicInfo,
    pub service_type: ServiceType,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,

    // RCoder 模式字段
    pub session_id: Option<String>,
    pub status: Option<AgentStatus>,
    pub model_provider: Option<ModelProviderConfig>,

    // ComputerAgentRunner 模式字段
    pub projects: Option<Arc<DashMap<String, Arc<ProjectInfo>>>>,
}

impl UnifiedContainerInfo {
    pub fn is_fully_idle(&self, idle_timeout: Duration) -> bool {
        let now = Utc::now();
        let idle_duration = now - self.last_activity;
        let is_timeout = idle_duration >
            chrono::Duration::from_std(idle_timeout).unwrap_or_default();

        match self.service_type {
            ServiceType::RCoder => {
                let is_idle_status = matches!(
                    self.status,
                    Some(AgentStatus::Idle) | None
                );
                is_idle_status && is_timeout
            }
            ServiceType::ComputerAgentRunner => {
                if let Some(projects) = &self.projects {
                    if projects.is_empty() { return true; }

                    projects.iter().all(|entry| {
                        let project = entry.value();
                        let project_idle_duration = now - project.last_activity;
                        let project_is_timeout = project_idle_duration >
                            chrono::Duration::from_std(idle_timeout).unwrap_or_default();
                        let is_idle_status = matches!(
                            project.status,
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
}
```

#### 步骤 1.3：实现 ProjectInfo 结构

**任务清单**:
- [ ] 定义 `ProjectInfo` 结构体（简化版项目元数据）
- [ ] 实现 `new()` 构造方法
- [ ] 实现 `update_activity()` 方法
- [ ] 实现 `update_session()` 方法
- [ ] 实现 `update_status()` 方法

**结构定义**:
```rust
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub project_id: String,
    pub session_id: Option<String>,
    pub status: Option<AgentStatus>,
    pub model_provider: Option<ModelProviderConfig>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}
```

#### 步骤 1.4：实现 SessionInfo 结构

**任务清单**:
- [ ] 定义 `SessionInfo` 结构体（统一会话信息）
- [ ] 实现 `new()` 构造方法
- [ ] 关联 `ContainerKey` 和 `project_id`

**结构定义**:
```rust
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub container_key: ContainerKey,
    pub project_id: String,
    pub created_at: DateTime<Utc>,
}
```

#### 步骤 1.5：更新模块导出

**文件**: `crates/shared_types/src/model/mod.rs`

**任务清单**:
- [ ] 添加 `mod computer_agent_model;`
- [ ] 添加 `pub use computer_agent_model::*;`

#### 验收标准

- [ ] 所有结构体编译通过，无警告
- [ ] `is_fully_idle()` 单元测试通过（RCoder 和 ComputerAgentRunner 两种模式）
- [ ] `ContainerKey` 可以正确序列化和反序列化
- [ ] 所有方法都有完整的文档注释
- [ ] cargo clippy 无警告

---

### 阶段 2：容器管理服务实现（1天，P0）

#### 目标
实现 `ComputerContainerManager` 服务，负责根据 `user_id` 获取或创建容器。

#### 步骤 2.1：创建 ComputerContainerManager

**文件**: `crates/rcoder/src/service/computer_container_manager.rs`

**任务清单**:
- [ ] 定义 `ComputerContainerManager` 结构体
- [ ] 实现 `get_or_create_container_for_user()` 公开方法
  - 输入：`user_id: &str`, `resource_limits: Option<ServiceResourceLimits>`
  - 输出：`Result<ContainerBasicInfo, AppError>`
  - 逻辑：查询是否存在 → 存在则返回 → 不存在则创建
- [ ] 实现 `create_container_for_user()` 私有方法
  - 准备用户级工作目录
  - 解析宿主机路径
  - 调用 `DockerManager::start_agent_container()`
- [ ] 实现 `get_user_workspace()` 方法
  - 返回 `/app/computer-project-workspace/{user_id}`
- [ ] 实现 `create_user_workspace()` 方法
  - 创建用户工作区目录
  - 确保目录权限正确

**关键代码模式**:
```rust
pub struct ComputerContainerManager;

impl ComputerContainerManager {
    pub async fn get_or_create_container_for_user(
        user_id: &str,
        resource_limits: Option<ServiceResourceLimits>,
    ) -> Result<ContainerBasicInfo, AppError> {
        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| AppError::internal_server_error(&format!("获取 DockerManager 失败: {}", e)))?;

        // 检查容器是否已存在
        if let Ok(Some(info)) = docker_manager.get_agent_info(user_id).await {
            return Ok(info);
        }

        // 创建新容器
        Self::create_container_for_user(user_id, &docker_manager, resource_limits).await
    }

    async fn create_container_for_user(
        user_id: &str,
        docker_manager: &Arc<docker_manager::DockerManager>,
        resource_limits: Option<ServiceResourceLimits>,
    ) -> Result<ContainerBasicInfo, AppError> {
        // 准备工作目录
        let user_workspace = Self::get_user_workspace(user_id).await?;
        Self::create_user_workspace(user_id).await?;

        // 解析宿主机路径
        let host_path = resolve_container_path_to_host(&user_workspace).await?;

        // 调用 DockerManager 启动容器
        let container_info = docker_manager
            .start_agent_container(
                user_id,
                &host_path.to_string_lossy(),
                ServiceType::ComputerAgentRunner,
                resource_limits,
            )
            .await?;

        Ok(container_info)
    }
}
```

#### 步骤 2.2：集成 DockerManager

**任务清单**:
- [ ] 调用 `docker_manager::global::get_global_docker_manager()` 获取全局实例
- [ ] 使用 `get_agent_info()` 查询现有容器
- [ ] 使用 `start_agent_container()` 创建新容器，传递：
  - `user_id` 作为容器标识
  - `host_path` 宿主机路径
  - `ServiceType::ComputerAgentRunner` 服务类型
  - `resource_limits` 资源限额配置

#### 步骤 2.3：路径管理

**任务清单**:
- [ ] 工作区路径：`/app/computer-project-workspace/{user_id}`
- [ ] 使用 `tokio::fs::create_dir_all()` 创建目录
- [ ] 使用 `resolve_container_path_to_host()` 解析宿主机路径
- [ ] 确保目录权限为 777（容器内外都可访问）

#### 步骤 2.4：更新服务模块导出

**文件**: `crates/rcoder/src/service/mod.rs`

**任务清单**:
- [ ] 添加 `pub mod computer_container_manager;`
- [ ] 添加 `pub use computer_container_manager::*;`

#### 验收标准

- [ ] 可以成功创建用户容器
- [ ] 容器命名规则正确：`computer-agent-runner-{user_id}`
- [ ] 工作区目录创建成功：`/app/computer-project-workspace/{user_id}`
- [ ] 容器 IP 地址可以正确获取
- [ ] 容器内外路径映射正确
- [ ] 集成测试通过（调用 API 创建容器）

---

### 阶段 3：HTTP 接口实现（1天，P0）

#### 目标
实现 Computer Agent 的 HTTP 接口，包括聊天、停止 Agent 和进度流。

#### 步骤 3.1：实现 computer_chat_handler

**文件**: `crates/rcoder/src/handler/computer_chat_handler.rs`

**任务清单**:
- [ ] 定义 `ComputerChatRequest` 结构体
  - `user_id: String` (必填)
  - `project_id: Option<String>` (可选，自动生成)
  - `prompt: String`
  - 其他字段与现有 `ChatRequest` 保持一致
- [ ] 实现 `handle_computer_chat()` 函数
  - 生成或使用提供的 `project_id`
  - 调用 `ComputerContainerManager::get_or_create_container_for_user()`
  - 获取或创建 `UnifiedContainerInfo` (ContainerKey::User)
  - 创建或更新 `ProjectInfo`
  - 通过 gRPC 转发请求到 agent_runner
  - 更新会话映射
  - 返回 `ChatResponse`
- [ ] 实现 `forward_computer_request_to_container()` 函数
  - 仅使用 gRPC 通信（不回退 HTTP）
  - 调用 `grpc_pool.get_or_create_channel()`
  - 调用 `Chat` RPC
- [ ] 实现 `computer_session_notification()` SSE 处理器
  - 通过 `sessions` 查找 `SessionInfo`
  - 建立 SSE 连接到容器
  - 实时推送进度事件

**请求处理流程**:
```
POST /computer/chat
    ↓
1. 验证 user_id
2. 生成 project_id（若未提供）
3. get_or_create_container_for_user(user_id)
4. 获取或创建 UnifiedContainerInfo (ContainerKey::User)
5. 创建/更新 ProjectInfo
6. gRPC Chat RPC → agent_runner (带 project_id)
7. 更新会话映射 (session_id → SessionInfo)
8. 返回 ChatResponse
```

#### 步骤 3.2：实现 computer_agent_stop_handler

**文件**: `crates/rcoder/src/handler/computer_agent_stop_handler.rs`

**任务清单**:
- [ ] 定义 `ComputerAgentStopRequest` 结构体
  - `user_id: String` (必填)
  - `project_id: String` (必填)
  - `session_id: Option<String>` (可选)
- [ ] 实现 `computer_agent_stop()` 函数
  - 查找 `containers[ContainerKey::User(user_id)]`
  - 从 `container_info.projects` 移除 `project_id`
  - 通过 gRPC StopAgent RPC 停止 agent
  - 清理会话映射
  - 返回成功响应

**注意**: 容器不会被销毁，继续运行其他 `project_id`。

#### 步骤 3.3：添加路由

**文件**: `crates/rcoder/src/router.rs`

**任务清单**:
- [ ] 添加 `/computer/chat` 路由：`post(handler::handle_computer_chat)`
- [ ] 添加 `/computer/agent/stop` 路由：`post(handler::computer_agent_stop)`
- [ ] 添加 `/computer/progress/{session_id}` 路由：`get(handler::computer_session_notification)`

#### 步骤 3.4：重构 AppState 使用统一架构

**文件**: `crates/rcoder/src/router.rs`

**任务清单**:
- [ ] 重构 `AppState` 结构体，从 6 个 DashMap 精简到 3 个：
  ```rust
  pub struct AppState {
      pub config: AppConfig,

      // 核心映射（统一管理）
      pub containers: DashMap<ContainerKey, Arc<UnifiedContainerInfo>>,
      pub sessions: DashMap<String, Arc<SessionInfo>>,
      pub project_to_container: DashMap<String, ContainerKey>,

      pub pingora_service: Option<Arc<pingora_proxy::PingoraProxyService>>,
      pub grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
  }
  ```
- [ ] 实现便捷方法：
  - `get_container(&self, key: &ContainerKey)`: 获取容器信息
  - `get_container_by_project(&self, project_id: &str)`: 通过 project_id 获取容器
  - `get_session(&self, session_id: &str)`: 获取会话信息
  - `upsert_container(&self, key, info)`: 添加或更新容器
  - `add_session(&self, session_id, info)`: 添加会话
  - `remove_container(&self, key)`: 统一清理方法（清理所有相关映射）

**关键代码模式**:
```rust
impl AppState {
    pub fn remove_container(&self, key: &ContainerKey) -> Option<Arc<UnifiedContainerInfo>> {
        // 移除容器
        let container = self.containers.remove(key).map(|(_, v)| v)?;

        // 清理 project_to_container 索引
        match key {
            ContainerKey::Project(project_id) => {
                self.project_to_container.remove(project_id);
            }
            ContainerKey::User(_) => {
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

#### 步骤 3.5：更新处理器模块导出

**文件**: `crates/rcoder/src/handler/mod.rs`

**任务清单**:
- [ ] 添加 `pub mod computer_chat_handler;`
- [ ] 添加 `pub mod computer_agent_stop_handler;`
- [ ] 添加 `pub use computer_chat_handler::*;`
- [ ] 添加 `pub use computer_agent_stop_handler::*;`

#### 验收标准

- [ ] `POST /computer/chat` 接口可用，返回正确的 `ChatResponse`
- [ ] 自动创建 `user_id` 对应的容器（首次请求）
- [ ] `project_id` 自动生成（若未提供）
- [ ] SSE 进度流正常工作（`GET /computer/progress/{session_id}`）
- [ ] 可以停止特定 `project_id` 的 agent（`POST /computer/agent/stop`）
- [ ] AppState 重构后所有现有接口仍然正常工作
- [ ] 集成测试通过（发送聊天请求 → 接收进度 → 停止 agent）

---

### 阶段 4：agent_runner 集成 agent_abstraction（2-3天，P0）

#### 目标
复用现有的 `agent_abstraction` 模块，避免重复代码，实现多 Agent 实例管理。

#### 步骤 4.1：扩展 gRPC Proto

**文件**: `crates/shared_types/proto/agent.proto`

**任务清单**:
- [ ] 添加 `StopAgent` RPC 定义
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
  ```
- [ ] 添加 `StopAgentRequest` 消息定义
  ```protobuf
  message StopAgentRequest {
      string project_id = 1;
  }
  ```
- [ ] 添加 `StopAgentResponse` 消息定义
  ```protobuf
  message StopAgentResponse {
      bool success = 1;
      string message = 2;
      string project_id = 3;
  }
  ```
- [ ] 运行 `cargo build` 重新生成 Proto 代码

#### 步骤 4.2：修改 agent_runner 主程序

**文件**: `crates/agent_runner/src/main.rs`

**任务清单**:
- [ ] 引入 `agent_abstraction` 模块：
  ```rust
  use agent_abstraction::{
      AcpSessionManager,
      AgentLifecycleManager,
      AcpAgentWorker,
  };
  ```
- [ ] 创建 `AcpSessionManager` 实例（多会话管理）
- [ ] 创建 `AgentLifecycleManager` 实例（生命周期管理）
- [ ] 创建 `AcpAgentWorker` 实例（Worker 模式）
- [ ] 在 `LocalSet` 中初始化所有组件
- [ ] 将实例传递给 gRPC 服务实现

**关键代码模式**:
```rust
struct AgentRunnerState<N: SessionNotifier, C: Client + 'static> {
    session_manager: Arc<AcpSessionManager<N, C>>,
    lifecycle_manager: Arc<AgentLifecycleManager>,
    worker: Arc<AcpAgentWorker<N, C>>,
}

impl<N: SessionNotifier, C: Client + 'static> AgentRunnerState<N, C> {
    pub fn new(
        notifier: Arc<N>,
        lifecycle_manager: Arc<AgentLifecycleManager>,
    ) -> Self {
        let session_manager = Arc::new(AcpSessionManager::new(notifier));
        let worker = Arc::new(AcpAgentWorker::new(session_manager.clone()));

        Self {
            session_manager,
            lifecycle_manager,
            worker,
        }
    }
}
```

#### 步骤 4.3：修改 gRPC 服务实现

**文件**: `crates/agent_runner/src/grpc/agent_service_impl.rs`

**任务清单**:
- [ ] 修改 `Chat` RPC 实现：
  - 调用 `worker.process_request()` 处理请求
  - `project_id` 自动路由到对应的 agent 实例
- [ ] 实现 `StopAgent` RPC：
  - 从 `session_manager` 移除会话
  - 调用 `lifecycle_manager.stop_agent(project_id)`
  - 清理 Agent 资源
  - 返回 `StopAgentResponse`
- [ ] 确保所有操作在 `LocalSet` 中执行（ACP 协议要求）

**关键代码模式**:
```rust
#[tonic::async_trait]
impl<N: SessionNotifier, C: Client + 'static> AgentService for AgentServiceImpl<N, C> {
    async fn chat(
        &self,
        request: Request<GrpcChatRequest>,
    ) -> Result<Response<GrpcChatResponse>, Status> {
        let req = request.into_inner();

        // 构建 WorkerRequest
        let worker_request = WorkerRequest {
            prompt_message: /* 转换 */,
            model_provider: req.model_provider,
            attachment_blocks: /* 转换 */,
        };

        // 调用 worker 处理（自动路由到 project_id）
        let response = self.state.worker.process_request(worker_request)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(GrpcChatResponse {
            session_id: response.session_id,
            project_id: response.project_id,
            // ...
        }))
    }

    async fn stop_agent(
        &self,
        request: Request<StopAgentRequest>,
    ) -> Result<Response<StopAgentResponse>, Status> {
        let req = request.into_inner();

        // 从会话管理器移除
        self.state.session_manager.remove_session(&req.project_id);

        // 停止 agent 进程
        self.state.lifecycle_manager.stop_agent(&req.project_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(StopAgentResponse {
            success: true,
            message: format!("Agent {} stopped", req.project_id),
            project_id: req.project_id,
        }))
    }
}
```

#### 步骤 4.4：测试多 Agent 实例

**任务清单**:
- [ ] 在同一容器内启动多个 `project_id` 的 agent
- [ ] 验证 Agent 之间互不干扰（独立的工作区和会话）
- [ ] 验证可以独立停止单个 Agent（容器继续运行）
- [ ] 验证内存和资源使用合理（无泄漏）

#### 验收标准

- [ ] 同一容器内可以运行多个 `project_id` 的 agent
- [ ] Agent 之间无上下文污染（独立的 `AcpSessionManager` 会话）
- [ ] 可以按 `project_id` 停止单个 agent
- [ ] 容器不会因停止单个 agent 而销毁
- [ ] `StopAgent` RPC 测试通过
- [ ] 集成测试通过（多 agent 并发）
- [ ] 内存泄漏测试通过

---

### 阶段 5：闲置检测和清理优化（1天，P1）

#### 目标
实现统一的闲置检测和清理逻辑，自动处理 RCoder 和 ComputerAgentRunner 两种模式。

#### 步骤 5.1：修改清理任务

**文件**: `crates/rcoder/src/proxy_agent/cleanup_task.rs`

**任务清单**:
- [ ] 重构 `cleanup_idle_agents()` 方法，使用统一逻辑：
  ```rust
  async fn cleanup_idle_agents(&mut self) -> Result<CleanupStats> {
      let mut stats = CleanupStats::default();
      let current_time = Utc::now();
      let mut containers_to_clean = Vec::new();

      for entry in self.state.containers.iter() {
          let container_key = entry.key();
          let container_info = entry.value();

          // 容器保护期（创建后 5 分钟内不清理）
          let protection_time = chrono::Duration::minutes(5);
          if current_time - container_info.created_at < protection_time {
              continue;
          }

          // 使用统一的闲置判断方法
          if container_info.is_fully_idle(self.config.idle_timeout) {
              containers_to_clean.push(container_key.clone());
          }
      }

      // 执行清理
      for container_key in containers_to_clean {
          match self.cleanup_container(&container_key).await {
              Ok(_) => stats.cleaned_count += 1,
              Err(e) => {
                  stats.failed_count += 1;
                  warn!("清理失败: {} - {}", container_key, e);
              }
          }
      }

      Ok(stats)
  }
  ```
- [ ] 实现 `cleanup_container()` 统一清理方法：
  - 获取容器信息
  - 销毁 Docker 容器
  - 清理 gRPC 连接池
  - 调用 `AppState::remove_container()` 清理所有映射
- [ ] 添加清理日志（记录闲置时长、项目数量等）

#### 步骤 5.2：实现容器保护期

**任务清单**:
- [ ] 容器创建后 5 分钟内不进行清理
- [ ] 检查 `container_info.created_at` 时间
- [ ] 记录保护期跳过的日志

#### 步骤 5.3：实现孤立容器检测

**任务清单**:
- [ ] 列出所有 `rcoder-agent-*` 和 `computer-agent-runner-*` 容器
- [ ] 与 `AppState.containers` 对比，找出孤立容器
- [ ] 清理不在 AppState 中的容器
- [ ] 记录孤立容器清理日志

**关键代码模式**:
```rust
async fn cleanup_orphaned_containers(&mut self) -> u64 {
    let mut cleaned_count = 0;
    let docker_manager = /* 获取 DockerManager */;

    // 收集所有应该存在的容器 ID
    let expected_containers: HashSet<String> = self.state.containers
        .iter()
        .map(|entry| entry.value().container.container_id.clone())
        .collect();

    // 列出所有容器
    let patterns = vec!["rcoder-agent-", "computer-agent-runner-"];
    for pattern in patterns {
        if let Ok(containers) = docker_manager.list_containers_by_pattern(pattern).await {
            for container in containers {
                if !expected_containers.contains(&container.id) {
                    // 清理孤立容器
                    if let Err(e) = docker_manager.stop_container_by_id(&container.id).await {
                        warn!("清理孤立容器失败: {} - {}", container.id, e);
                    } else {
                        cleaned_count += 1;
                    }
                }
            }
        }
    }

    cleaned_count
}
```

#### 步骤 5.4：添加清理日志

**任务清单**:
- [ ] 记录清理操作（容器 key、清理原因）
- [ ] 记录闲置时长（`now - last_activity`）
- [ ] ComputerAgentRunner 模式：记录项目数量和各项目状态
- [ ] 记录清理成功/失败统计

#### 验收标准

- [ ] RCoder 模式：`project_id` 闲置即清理
- [ ] ComputerAgentRunner 模式：所有 `project_id` 都闲置才清理
- [ ] 容器保护期生效（创建后 5 分钟内不清理）
- [ ] 孤立容器被正确清理
- [ ] 清理日志完整且可读
- [ ] 清理统计准确（`CleanupStats`）
- [ ] 单元测试覆盖 `is_fully_idle()` 各种场景

---

### 阶段 6：VNC 代理实现（1-2天，P1）

#### 目标
实现 VNC 桌面访问功能，提供 WebSocket 透明代理。

#### 步骤 6.1：创建 computer_desktop_handler

**文件**: `crates/rcoder/src/handler/computer_desktop_handler.rs`

**任务清单**:
- [ ] 实现 `computer_desktop_vnc()` 函数：
  ```rust
  pub async fn computer_desktop_vnc(
      State(state): State<Arc<AppState>>,
      Path((user_id, project_id)): Path<(String, String)>,
  ) -> Result<Response, AppError> {
      // 1. 查找 containers[ContainerKey::User(user_id)]
      // 2. 获取容器 IP
      // 3. 构建目标 URL: ws://{container_ip}:6080
      // 4. 使用 Pingora/Nginx 代理 WebSocket 连接
  }
  ```
- [ ] 查找 `user_id` 对应的容器
- [ ] 获取容器 IP 地址
- [ ] 构建 WebSocket 代理 URL

#### 步骤 6.2：实现 WebSocket 代理（方案选择）

**方案 A（推荐）：Pingora WebSocket 中间件**

**任务清单**:
- [ ] 调研 Pingora WebSocket 支持情况
- [ ] 实现 HTTP Upgrade 处理
- [ ] 透明转发 WebSocket 帧到容器的 6080 端口
- [ ] 处理连接错误和超时

**方案 B（备用）：Nginx 反向代理**

**任务清单**:
- [ ] 配置 Nginx location：
  ```nginx
  location ~ ^/computer/desktop/([^/]+)/([^/]+)$ {
      set $user_id $1;
      # 动态查询容器 IP（通过 Lua 脚本或 upstream 模块）
      proxy_pass http://$container_ip:6080;
      proxy_http_version 1.1;
      proxy_set_header Upgrade $http_upgrade;
      proxy_set_header Connection "upgrade";
      proxy_set_header Host $host;
      proxy_set_header X-Real-IP $remote_addr;
  }
  ```
- [ ] 实现动态 upstream 配置

**方案 C（临时）：直接返回容器 URL**

**任务清单**:
- [ ] 返回 `http://{container_ip}:6080/vnc.html` 给前端
- [ ] 仅用于开发测试

#### 步骤 6.3：添加 VNC 路由

**文件**: `crates/rcoder/src/router.rs`

**任务清单**:
- [ ] 添加 `/computer/desktop/:user_id/:project_id` 路由
- [ ] 使用 `get(handler::computer_desktop_vnc)` 处理器

#### 步骤 6.4：测试 VNC 访问

**任务清单**:
- [ ] 创建测试 HTML 页面（`fixtures/vnc-test.html`）：
  ```html
  <!DOCTYPE html>
  <html>
  <head>
      <title>VNC Desktop Test</title>
  </head>
  <body>
      <h1>Computer Agent VNC Desktop</h1>
      <form id="vnc-form">
          <label>User ID: <input type="text" id="user_id" value="test_user_1"></label><br>
          <label>Project ID: <input type="text" id="project_id" value="proj_1"></label><br>
          <label>Server URL: <input type="text" id="server" value="http://localhost:8087"></label><br>
          <button type="submit">Connect</button>
      </form>
      <iframe id="vnc-frame" width="100%" height="800"></iframe>
      <script>
          document.getElementById('vnc-form').addEventListener('submit', function(e) {
              e.preventDefault();
              const userId = document.getElementById('user_id').value;
              const projectId = document.getElementById('project_id').value;
              const server = document.getElementById('server').value;
              const vncUrl = `${server}/computer/desktop/${userId}/${projectId}`;
              document.getElementById('vnc-frame').src = vncUrl;
          });
      </script>
  </body>
  </html>
  ```
- [ ] 验证 WebSocket 连接建立
- [ ] 验证桌面画面传输流畅
- [ ] 测试鼠标和键盘输入

#### 验收标准

- [ ] 可以通过浏览器访问 VNC 桌面
- [ ] WebSocket 连接稳定（不频繁断开）
- [ ] 桌面画面流畅（延迟 < 500ms）
- [ ] `user_id` 隔离生效（不同用户互不干扰）
- [ ] 测试 HTML 页面可用
- [ ] 性能测试通过（多用户并发访问）

---

### 阶段 7：MCP 配置和优化（0.5天，P2）

#### 目标
创建 ComputerAgentRunner 专用的 MCP 配置文件。

#### 步骤 7.1：创建配置文件

**文件**: `crates/agent_config/configs/computer_agent_default.json`

**任务清单**:
- [ ] 复制 `default_agents.json` 内容作为基础
- [ ] 添加 Chrome DevTools MCP 配置：
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
- [ ] 设置 `CHROME_REMOTE_DEBUGGING_PORT=9222`

#### 步骤 7.2：配置加载逻辑

**任务清单**:
- [ ] 修改配置加载代码，根据 `ServiceType` 选择配置文件：
  ```rust
  fn get_agent_config_path(service_type: ServiceType) -> PathBuf {
      match service_type {
          ServiceType::RCoder => PathBuf::from("configs/default_agents.json"),
          ServiceType::ComputerAgentRunner => PathBuf::from("configs/computer_agent_default.json"),
      }
  }
  ```
- [ ] 确保配置文件正确加载

#### 步骤 7.3：验证 MCP 工具

**任务清单**:
- [ ] 启动 agent 后验证 MCP 工具加载日志
- [ ] 测试 Chrome DevTools MCP 功能：
  - 导航到网页
  - 截图
  - 元素操作
- [ ] 测试浏览器操作能力（agent 可以控制 Chromium）

#### 验收标准

- [ ] 配置文件格式正确（JSON 验证通过）
- [ ] Chrome DevTools MCP 加载成功（日志显示连接到 CDP）
- [ ] Agent 可以操作 Chromium 浏览器
- [ ] 支持网页导航和元素操作
- [ ] 支持截图功能
- [ ] 配置文件文档完整（metadata 字段）

---

## 四、集成测试

### 4.1 功能测试用例

#### 测试用例 1：基本聊天流程

**目标**: 验证 Computer Agent 的基本聊天功能

**步骤**:
```bash
# 1. 发送聊天请求
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "test_user_1",
    "prompt": "Hello, help me create a simple React app"
  }'

# 预期输出：
# {
#   "success": true,
#   "data": {
#     "session_id": "sess_xxx",
#     "project_id": "proj_yyy",
#     "message": "Task started"
#   }
# }

# 2. 验证容器创建
docker ps | grep computer-agent-runner-test_user_1

# 预期输出：显示一个运行中的容器

# 3. 订阅进度流
curl http://localhost:8087/computer/progress/{session_id}

# 预期输出：SSE 事件流，显示 agent 执行进度
```

**验收标准**:
- [ ] API 返回正确的响应（`session_id` 和 `project_id`）
- [ ] 容器自动创建（命名正确）
- [ ] SSE 进度流实时推送事件

#### 测试用例 2：多项目并发

**目标**: 验证同一用户下多个项目可以并发运行

**步骤**:
```bash
# 在同一用户下创建多个项目
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{"user_id": "test_user_1", "project_id": "proj_1", "prompt": "Task 1"}'

curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{"user_id": "test_user_1", "project_id": "proj_2", "prompt": "Task 2"}'

# 验证容器数量（应该只有 1 个）
docker ps | grep computer-agent-runner-test_user_1 | wc -l

# 预期输出：1

# 验证两个项目都在运行
# （通过查看日志或 agent 状态）
```

**验收标准**:
- [ ] 只创建一个容器
- [ ] 两个项目的 agent 都在运行
- [ ] 项目之间互不干扰（独立的工作区）

#### 测试用例 3：Agent 停止

**目标**: 验证可以停止单个项目的 agent，而不销毁容器

**步骤**:
```bash
# 停止特定项目的 agent
curl -X POST http://localhost:8087/computer/agent/stop \
  -H "Content-Type: application/json" \
  -d '{"user_id": "test_user_1", "project_id": "proj_1"}'

# 预期输出：
# {
#   "success": true,
#   "message": "Agent stopped"
# }

# 验证容器仍然运行（proj_2 还在）
docker ps | grep computer-agent-runner-test_user_1

# 预期输出：容器仍在运行
```

**验收标准**:
- [ ] proj_1 的 agent 停止
- [ ] 容器仍然运行
- [ ] proj_2 的 agent 不受影响

#### 测试用例 4：VNC 访问

**目标**: 验证可以通过浏览器访问 VNC 桌面

**步骤**:
```bash
# 访问 VNC 桌面
open http://localhost:8087/computer/desktop/test_user_1/proj_1

# 或使用测试 HTML 页面
open fixtures/vnc-test.html
```

**验收标准**:
- [ ] 浏览器显示 VNC 桌面
- [ ] 可以看到 XFCE4 桌面环境
- [ ] 鼠标和键盘输入正常
- [ ] 画面流畅（无明显延迟）

#### 测试用例 5：闲置清理

**目标**: 验证闲置清理逻辑正确

**步骤**:
```bash
# 1. 启动两个项目
curl -X POST http://localhost:8087/computer/chat \
  -d '{"user_id": "test_user_1", "project_id": "proj_1", "prompt": "Task 1"}'

curl -X POST http://localhost:8087/computer/chat \
  -d '{"user_id": "test_user_1", "project_id": "proj_2", "prompt": "Task 2"}'

# 2. 停止所有 agent
curl -X POST http://localhost:8087/computer/agent/stop \
  -d '{"user_id": "test_user_1", "project_id": "proj_1"}'

curl -X POST http://localhost:8087/computer/agent/stop \
  -d '{"user_id": "test_user_1", "project_id": "proj_2"}'

# 3. 等待闲置超时（默认 30 分钟，测试时可改为 1 分钟）
sleep 70

# 4. 验证容器被销毁
docker ps | grep computer-agent-runner-test_user_1

# 预期输出：无容器运行
```

**验收标准**:
- [ ] 只有当所有项目都闲置时才清理容器
- [ ] 闲置超时时间准确
- [ ] 容器保护期生效（创建后 5 分钟内不清理）

### 4.2 性能测试

#### 测试场景 1：容器创建时间

**目标**: 验证容器创建时间 < 30 秒

**步骤**:
```bash
time (curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{"user_id": "test_user_perf", "prompt": "Hello"}')
```

**验收标准**:
- [ ] 容器创建时间 < 30 秒
- [ ] 记录实际耗时

#### 测试场景 2：多 Agent 并发

**目标**: 验证单个容器可以稳定运行 3+ 个 agent

**步骤**:
```bash
# 并发启动 3 个 agent
for i in {1..3}; do
  curl -X POST http://localhost:8087/computer/chat \
    -d "{\"user_id\": \"test_user_1\", \"project_id\": \"proj_$i\", \"prompt\": \"Task $i\"}" &
done
wait

# 监控资源使用
docker stats computer-agent-runner-test_user_1
```

**验收标准**:
- [ ] 3 个 agent 都成功启动
- [ ] 响应时间合理（< 2 秒）
- [ ] 内存占用 < 4GB
- [ ] CPU 占用 < 2 核

#### 测试场景 3：VNC 延迟

**目标**: 验证 VNC 桌面访问延迟 < 500ms

**步骤**:
- 使用浏览器开发工具测量 WebSocket 帧传输延迟
- 记录平均延迟、最大延迟

**验收标准**:
- [ ] 平均延迟 < 500ms
- [ ] 画面流畅，无明显卡顿

#### 测试场景 4：资源占用

**目标**: 验证容器资源占用符合限额

**步骤**:
```bash
# 查看容器资源使用
docker stats computer-agent-runner-test_user_1 --no-stream
```

**验收标准**:
- [ ] 内存占用 < 4GB（可配置）
- [ ] CPU 占用 < 2 核（可配置）
- [ ] 磁盘 IO 合理

### 4.3 安全测试

#### 测试场景 1：用户隔离

**目标**: 验证不同用户之间的隔离

**步骤**:
```bash
# 用户 A 创建项目
curl -X POST http://localhost:8087/computer/chat \
  -d '{"user_id": "user_a", "project_id": "proj_1", "prompt": "Task A"}'

# 用户 B 尝试访问用户 A 的容器
curl -X POST http://localhost:8087/computer/agent/stop \
  -d '{"user_id": "user_b", "project_id": "proj_1"}'

# 预期：失败（找不到容器）

# 用户 B 尝试访问用户 A 的 VNC
curl http://localhost:8087/computer/desktop/user_a/proj_1

# 预期：失败（无权限或找不到）
```

**验收标准**:
- [ ] user_a 无法访问 user_b 的容器
- [ ] user_a 无法访问 user_b 的 VNC
- [ ] 错误消息清晰

#### 测试场景 2：项目隔离

**目标**: 验证项目之间无上下文污染

**步骤**:
```bash
# 在两个项目中创建同名文件
# proj_1 创建 test.txt
curl -X POST http://localhost:8087/computer/chat \
  -d '{"user_id": "test_user_1", "project_id": "proj_1", "prompt": "echo proj1 > test.txt"}'

# proj_2 创建 test.txt
curl -X POST http://localhost:8087/computer/chat \
  -d '{"user_id": "test_user_1", "project_id": "proj_2", "prompt": "echo proj2 > test.txt"}'

# 验证文件内容不同
# （通过 agent 读取文件内容）
```

**验收标准**:
- [ ] 两个项目的 test.txt 文件内容不同
- [ ] 工作区目录独立（`/app/computer-project-workspace/{user_id}/{project_id}`）

#### 测试场景 3：资源限额

**目标**: 验证容器资源限额生效

**步骤**:
```bash
# 创建容器时设置资源限额
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "test_user_1",
    "prompt": "Task",
    "agent_config": {
      "resource_limits": {
        "memory_limit": 2147483648,
        "cpu_limit": 1.0
      }
    }
  }'

# 验证限额生效
docker inspect computer-agent-runner-test_user_1 | jq '.[0].HostConfig.Memory'
docker inspect computer-agent-runner-test_user_1 | jq '.[0].HostConfig.NanoCpus'
```

**验收标准**:
- [ ] 内存限额生效（Docker inspect 显示正确值）
- [ ] CPU 限额生效（Docker inspect 显示正确值）
- [ ] agent 在限额内正常运行

---

## 五、部署和上线

### 5.1 Docker 镜像更新

**检查清单**:
- [ ] `docker/rcoder-agent-runner/Dockerfile` 包含所有依赖
- [ ] XFCE4 桌面环境配置正确
- [ ] noVNC 服务启动脚本正确（端口 6080）
- [ ] Chromium 浏览器配置正确（CDP 端口 9222）
- [ ] 所有系统依赖已安装（Node.js, npm, bun, uv 等）
- [ ] 镜像大小合理（< 5GB）

### 5.2 Docker Compose 配置

**文件**: `docker/docker-compose.yml`

**检查清单**:
- [ ] 挂载了 `computer-project-workspace` 目录：
  ```yaml
  volumes:
    - ./project_workspace:/app/project_workspace
    - ./computer-project-workspace:/app/computer-project-workspace
    - ./logs:/app/logs
  ```
- [ ] 端口映射正确（8087:8087）
- [ ] 环境变量配置完整（ANTHROPIC_API_KEY, RUST_LOG 等）

### 5.3 环境变量配置

**必需环境变量**:
```bash
# API 密钥
ANTHROPIC_API_KEY=sk-ant-xxx

# 日志级别
RUST_LOG=info

# Docker 配置
DOCKER_SOCKET_PATH=/var/run/docker.sock
```

**可选环境变量**:
```bash
# 资源限额（默认值）
DEFAULT_MEMORY_LIMIT=4294967296  # 4GB
DEFAULT_CPU_LIMIT=2.0            # 2 核

# 闲置超时（默认值）
IDLE_TIMEOUT=1800  # 30 分钟
```

### 5.4 监控和告警

**监控指标**:
- [ ] 容器创建/销毁日志
- [ ] Agent 状态变更日志
- [ ] 资源使用情况监控（CPU、内存、磁盘）
- [ ] gRPC 连接池状态
- [ ] SSE 连接数量

**告警配置**:
- [ ] 容器创建失败告警
- [ ] 资源超限告警
- [ ] gRPC 连接失败告警
- [ ] 闲置清理失败告警

---

## 六、验收标准总结

### 功能验收

- [ ] 可以通过 `POST /computer/chat` 发送请求，自动创建 user_id 对应的容器
- [ ] 同一 user_id 的多个 project_id 可以在同一容器内运行
- [ ] 可以通过 `GET /computer/desktop/{user_id}/{project_id}` 访问 VNC 桌面
- [ ] Agent 可以通过 Chrome DevTools MCP 操作 Chromium 浏览器
- [ ] 可以通过 `POST /computer/agent/stop` 停止单个 project_id 的 agent（不销毁容器）
- [ ] 只有当 user_id 下所有 project_id 都闲置时才销毁容器

### 性能验收

- [ ] 单个容器可以稳定运行 3+ 个 project_id 的 agent
- [ ] VNC 桌面访问延迟 < 500ms
- [ ] 容器创建时间 < 30s

### 安全验收

- [ ] user_id 只能访问自己的容器和 VNC
- [ ] project_id 之间没有上下文污染
- [ ] 容器资源限额生效

---

## 七、文件清单

### 新建文件（6个）

1. **`crates/shared_types/src/model/computer_agent_model.rs`**
   - ContainerKey 枚举
   - UnifiedContainerInfo 结构
   - ProjectInfo 结构
   - SessionInfo 结构

2. **`crates/rcoder/src/service/computer_container_manager.rs`**
   - ComputerContainerManager 服务
   - 容器创建和管理逻辑

3. **`crates/rcoder/src/handler/computer_chat_handler.rs`**
   - ComputerChatRequest 结构
   - handle_computer_chat() 函数
   - forward_computer_request_to_container() 函数
   - computer_session_notification() SSE 处理器

4. **`crates/rcoder/src/handler/computer_agent_stop_handler.rs`**
   - ComputerAgentStopRequest 结构
   - computer_agent_stop() 函数

5. **`crates/rcoder/src/handler/computer_desktop_handler.rs`**
   - computer_desktop_vnc() 函数
   - VNC 代理逻辑

6. **`crates/agent_config/configs/computer_agent_default.json`**
   - ComputerAgentRunner 专用的 MCP 配置
   - Chrome DevTools MCP 配置

### 修改文件（8个）

1. **`crates/shared_types/src/model/mod.rs`**
   - 添加 computer_agent_model 模块导出

2. **`crates/rcoder/src/service/mod.rs`**
   - 添加 computer_container_manager 模块导出

3. **`crates/rcoder/src/handler/mod.rs`**
   - 添加 computer_chat_handler 模块导出
   - 添加 computer_agent_stop_handler 模块导出
   - 添加 computer_desktop_handler 模块导出

4. **`crates/rcoder/src/router.rs`**
   - 重构 AppState（从 6 个 DashMap 精简到 3 个）
   - 添加 Computer 相关路由
   - 实现便捷方法

5. **`crates/rcoder/src/proxy_agent/cleanup_task.rs`**
   - 统一清理逻辑
   - 容器保护期
   - 孤立容器检测

6. **`crates/agent_runner/src/main.rs`**
   - 集成 agent_abstraction 模块
   - 创建 AcpSessionManager 实例
   - 创建 AgentLifecycleManager 实例
   - 创建 AcpAgentWorker 实例

7. **`crates/agent_runner/src/grpc/agent_service_impl.rs`**
   - 修改 Chat RPC 实现
   - 实现 StopAgent RPC

8. **`crates/shared_types/proto/agent.proto`**
   - 添加 StopAgent RPC 定义
   - 添加 StopAgentRequest 消息
   - 添加 StopAgentResponse 消息

---

## 八、时间估算

| 阶段 | 工作量 | 依赖 | 关键风险 |
|------|--------|------|----------|
| 阶段 1：核心数据结构 | 1-2 天 | 无 | 低 |
| 阶段 2：容器管理服务 | 1 天 | 阶段 1 | 低 |
| 阶段 3：HTTP 接口 | 1 天 | 阶段 1, 2 | 低 |
| 阶段 4：agent_runner 集成 | 2-3 天 | 阶段 1 | 中（LocalSet 并发） |
| 阶段 5：闲置检测优化 | 1 天 | 阶段 1, 3 | 低 |
| 阶段 6：VNC 代理 | 1-2 天 | 阶段 3 | 中（WebSocket 代理） |
| 阶段 7：MCP 配置 | 0.5 天 | 阶段 4 | 低 |
| 集成测试 | 1 天 | 所有阶段 | 中 |
| **总计** | **8.5-11.5 天** | | |

---

## 九、风险和缓解措施

| 风险 | 影响 | 概率 | 缓解措施 |
|------|------|------|----------|
| agent_runner LocalSet 并发管理复杂 | 高 | 高 | 复用 agent_abstraction 模块，避免重复造轮 |
| Pingora 不支持 WebSocket | 中 | 中 | 备用方案：使用 Nginx 作为 VNC 专用代理 |
| 容器资源耗尽 | 高 | 中 | 实施严格的资源限额和监控 |
| VNC 安全风险 | 高 | 低 | user_id 绑定和访问控制（后续：一次性 token） |
| 多 agent 内存泄漏 | 中 | 中 | AgentLifecycleManager 管理，定期清理 |

---

## 十、后续优化（MVP 后）

### 第一阶段优化

1. **VNC 会话录制和回放**
   - 录制用户与桌面的交互
   - 回放功能用于调试和复现问题

2. **剪贴板 API 支持**
   - 实现剪贴板读取和写入接口
   - 支持跨容器的剪贴板共享

3. **增强 Agent 监控和日志**
   - 详细的性能指标（CPU、内存、网络）
   - Agent 操作日志（浏览器操作、文件操作等）

### 第二阶段扩展

4. **多用户协作（共享 VNC 桌面）**
   - 多个用户可以同时查看和操作同一个桌面
   - 实现光标同步和权限控制

5. **集成更多 MCP 工具**
   - 文件操作 MCP
   - 数据库操作 MCP
   - API 测试 MCP

6. **Agent 性能分析和优化**
   - 性能分析工具
   - 瓶颈识别
   - 优化建议

### 长期愿景

7. **Agent 市场**
   - 用户可以共享和复用 Agent 配置
   - 预置模板库
   - 社区贡献

8. **自定义桌面环境和应用**
   - 支持用户自定义桌面环境
   - 预装常用应用
   - 应用商店

9. **Agent 集群调度和负载均衡**
   - 多节点部署
   - 自动负载均衡
   - 高可用性保障

---

## 附录

### A. 术语表

| 术语 | 说明 |
|------|------|
| user_id | 用户唯一标识符 |
| project_id | 项目唯一标识符 |
| ContainerKey | 统一容器标识符枚举（Project/User） |
| UnifiedContainerInfo | 统一容器信息结构 |
| ProjectInfo | 项目信息结构 |
| SessionInfo | 会话信息结构 |
| ServiceType | 服务类型枚举（RCoder, ComputerAgentRunner） |
| ACP | Agent Client Protocol，AI Agent 通信协议 |
| MCP | Model Context Protocol，模型上下文协议 |
| CDP | Chrome DevTools Protocol，浏览器调试协议 |
| noVNC | 基于 HTML5 的 VNC 客户端 |
| LocalSet | Tokio 的本地任务集，支持 !Send 任务 |
| DashMap | 高性能的并发 HashMap |

### B. 参考资料

1. **rcoder 项目文档**:
   - `CLAUDE.md` - 项目概述
   - `specs/computer-agent-runner/0001-spec-claude.md` - 需求与设计文档

2. **外部依赖文档**:
   - [ACP Protocol](https://github.com/anthropics/agent-client-protocol)
   - [Chrome DevTools MCP](https://github.com/ChromeDevTools/chrome-devtools-mcp)
   - [noVNC Documentation](https://github.com/novnc/noVNC)
   - [Tonic gRPC](https://github.com/hyperium/tonic)
   - [DashMap](https://github.com/xacrimon/dashmap)

3. **技术博客**:
   - Rust Tokio LocalSet 使用指南
   - WebSocket 代理实现最佳实践
   - Docker 容器资源限制和管理

---

**文档变更历史**

| 版本 | 日期 | 作者 | 变更说明 |
|------|------|------|----------|
| v1.0 | 2025-12-10 | Claude | 初始版本，基于 0001-spec-claude.md 生成 |

---

**审批记录**

| 角色 | 姓名 | 审批意见 | 日期 |
|------|------|----------|------|
| 产品负责人 | | | |
| 技术负责人 | | | |
| 安全负责人 | | | |

---

**下一步行动**

1. **立即开始**: 阶段 1 - 核心数据结构实现
2. **并行开发**: 阶段 2 和阶段 3（容器管理 + HTTP 接口）
3. **关键里程碑**: 阶段 4 - agent_runner 集成（最复杂）
4. **集成测试**: 完成所有阶段后进行完整测试
5. **部署上线**: 通过验收后部署到生产环境

**联系方式**

- 技术支持: [技术团队邮箱]
- 项目管理: [项目经理邮箱]
- 文档反馈: [文档维护者邮箱]
