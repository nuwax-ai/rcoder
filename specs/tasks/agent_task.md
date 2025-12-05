# Agent 抽象层开发任务清单

> 基于 `specs/agent-abstraction-layer-design.md` 设计文档拆分的开发任务
> 
> **目标**：将 Agent、MCP 服务器、提示词从硬编码改为配置驱动的可扩展架构

---

## 任务总览

| 阶段 | 名称 | 任务数 | 依赖 | 优先级 |
|------|------|--------|------|--------|
| P0 | 基础类型定义 | 4 | 无 | 最高 |
| P1 | 配置管理系统 | 5 | P0 | 高 |
| P2 | Agent 抽象层核心 | 4 | P0 | 高 |
| P3 | 进程管理和生命周期 | 4 | P2 | 中 |
| P4 | ACP 连接池管理 | 3 | P2, P3 | 中 |
| P5 | MCP 服务器管理 | 3 | P1 | 中 |
| P6 | Agent 工厂和注册表 | 3 | P2, P4, P5 | 中 |
| P7 | 兼容层和迁移 | 3 | P6 | 高 |
| P8 | 集成和测试 | 3 | P7 | 高 |

---

## P0: 基础类型定义

### P0-1: 创建 `agent_config` crate 基础结构

**描述**: 创建新的 `crates/agent_config` crate，定义项目的基础目录结构

**输出产物**:
- `crates/agent_config/Cargo.toml`
- `crates/agent_config/src/lib.rs`
- `crates/agent_config/src/types/mod.rs`

**验收标准**:
- [ ] crate 可以被 workspace 正确引用
- [ ] `cargo check -p agent_config` 通过

**设计文档参考**: 4.3.1 Agent 配置结构

---

### P0-2: 定义 Agent 配置核心类型

**描述**: 定义 `AgentSpec`、`AgentConfig`、`SystemPromptConfig`、`UserPromptConfig` 等核心配置结构

**输出产物**:
- `crates/agent_config/src/types/agent_spec.rs` - AgentSpec 结构
- `crates/agent_config/src/types/agent_config.rs` - AgentConfig 结构
- `crates/agent_config/src/types/prompt_config.rs` - 提示词配置结构

**核心结构定义**:
```
AgentSpec:
  - agent_id: String
  - agent_type: AgentType
  - command: String
  - args: Vec<String>
  - env: HashMap<String, String>
  - installation: InstallationConfig
  - system_prompt: Option<SystemPromptConfig>
  - user_prompt: Option<UserPromptConfig>
  - enabled: bool
  - metadata: HashMap<String, String>

SystemPromptConfig:
  - template: String
  - enabled: bool (default: true)

UserPromptConfig:
  - template: String
  - enabled: bool (default: true)
```

**验收标准**:
- [ ] 所有结构体实现 `Debug, Clone, Serialize, Deserialize`
- [ ] 提供合理的 `Default` 实现
- [ ] 单元测试覆盖序列化/反序列化

**设计文档参考**: 4.2.3 Agent 规范定义, L241-303

---

### P0-3: 定义 MCP 服务器配置类型

**描述**: 定义 `McpServerConfig`、`McpServerSource`、`ContextServerConfig` 等 MCP 相关类型

**输出产物**:
- `crates/agent_config/src/types/mcp_config.rs`

**核心结构定义**:
```
McpServerConfig:
  - name: String
  - source: McpServerSource
  - enabled: bool
  - command: Option<String>
  - args: Option<Vec<String>>
  - env: Option<HashMap<String, String>>
  - timeout: Option<Duration>

McpServerSource:
  - Custom
  - Local

ContextServerConfig:
  - source: String
  - enabled: bool
  - command: Option<String>
  - args: Option<Vec<String>>
  - env: Option<HashMap<String, String>>
```

**验收标准**:
- [ ] 支持 JSON 序列化/反序列化
- [ ] 提供 `enabled` 过滤方法
- [ ] 单元测试覆盖各种配置场景

**设计文档参考**: 4.3.1 Agent 配置结构, L504-544

---

### P0-4: 定义安装配置和错误类型

**描述**: 定义 `InstallationConfig`、`PackageManager`、`AgentError` 等辅助类型

**输出产物**:
- `crates/agent_config/src/types/installation.rs`
- `crates/agent_config/src/error.rs`

**核心结构定义**:
```
InstallationConfig:
  - package_manager: PackageManager
  - package_name: Option<String>
  - version: Option<String>
  - source: Option<String>
  - validate_command: Option<Vec<String>>
  - auto_update: bool

PackageManager:
  - Npm
  - Local
  - Custom(String)

AgentError (thiserror):
  - StartupFailed(String)
  - ProcessError(String)
  - ConfigurationError(String)
  - ConnectionError(String)
  - Io(std::io::Error)
  - Other(String)
```

**验收标准**:
- [ ] `AgentError` 实现 `std::error::Error`
- [ ] 错误信息清晰，支持中文描述
- [ ] 单元测试覆盖错误转换

**设计文档参考**: 4.2.1 Agent 抽象 Trait, L188-206

---

## P1: 配置管理系统

### P1-1: 实现 AgentServersConfig 配置文件解析

**描述**: 实现主配置文件结构 `AgentServersConfig`，支持从 JSON 文件加载

**输出产物**:
- `crates/agent_config/src/config/mod.rs`
- `crates/agent_config/src/config/servers_config.rs`

**核心结构定义**:
```
AgentServersConfig:
  - agent_servers: HashMap<String, AgentServerConfig>
  - context_servers: HashMap<String, ContextServerConfig>

方法:
  - async fn from_file(path: &Path) -> Result<Self>
  - fn from_json(json: &str) -> Result<Self>
  - fn validate(&self) -> Result<()>
  - fn get_enabled_agents(&self) -> Vec<&AgentServerConfig>
  - fn get_agent(&self, agent_id: &str) -> Option<&AgentServerConfig>
```

**验收标准**:
- [ ] 支持从文件路径加载配置
- [ ] 支持配置校验（必填字段检查）
- [ ] 提供示例配置文件 `examples/agents.json`
- [ ] 单元测试覆盖解析成功和失败场景

**设计文档参考**: 4.4.4 Agent 配置和管理模块, L1002-1017

---

### P1-2: 实现环境变量解析器 EnvironmentVariableResolver

**描述**: 实现 `{MODEL_PROVIDER_*}` 等占位符的解析和替换

**输出产物**:
- `crates/agent_config/src/resolver/mod.rs`
- `crates/agent_config/src/resolver/env_resolver.rs`

**核心功能**:
```
EnvironmentVariableResolver:
  - fn with_standard_mappings() -> Self
  - fn resolve_agent_config(&self, config: &mut AgentConfig, model_provider: &ModelProviderConfig, project_context: &ProjectContext) -> Result<()>
  - fn resolve_value(&self, template: &str, context: &ResolutionContext) -> String
  - fn resolve_system_prompt(&self, config: &Option<SystemPromptConfig>, context: &ResolutionContext) -> Option<String>
  - fn resolve_user_prompt(&self, user_input: &str, config: &Option<UserPromptConfig>) -> String
  - fn add_mapping(&mut self, key: String, value: String)

标准映射:
  - {MODEL_PROVIDER_ID} -> ModelProviderConfig::id
  - {MODEL_PROVIDER_NAME} -> ModelProviderConfig::name
  - {MODEL_PROVIDER_BASE_URL} -> ModelProviderConfig::base_url
  - {MODEL_PROVIDER_API_KEY} -> ModelProviderConfig::api_key
  - {MODEL_PROVIDER_DEFAULT_MODEL} -> ModelProviderConfig::default_model
  - {MODEL_PROVIDER_API_PROTOCOL} -> ModelProviderConfig::api_protocol
  - {PROJECT_ID} -> ProjectContext::project_id
  - {PROJECT_PATH} -> ProjectContext::project_path
```

**验收标准**:
- [ ] 支持所有 ModelProviderConfig 字段映射
- [ ] 支持项目上下文变量
- [ ] 支持自定义变量添加
- [ ] 未知变量保持原样不替换
- [ ] 单元测试覆盖各种替换场景

**设计文档参考**: 4.3.0 环境变量映射系统, L403-465

---

### P1-3: 实现 ResolutionContext 上下文结构

**描述**: 定义变量解析所需的上下文信息

**输出产物**:
- `crates/agent_config/src/resolver/context.rs`

**核心结构定义**:
```
ResolutionContext:
  - model_provider: ModelProviderConfig
  - project_context: ProjectContext
  - custom_variables: HashMap<String, String>
  - mcp_variables: HashMap<String, String>

ProjectContext:
  - project_id: String
  - project_name: String
  - project_path: PathBuf
```

**验收标准**:
- [ ] 提供 Builder 模式构建
- [ ] 支持从现有 ChatPrompt 构建
- [ ] 单元测试

**设计文档参考**: L1140-1145

---

### P1-4: 实现默认配置生成器 DefaultConfigGenerator

**描述**: 实现 claude-code-acp 的默认配置自动生成

**输出产物**:
- `crates/agent_config/src/generator/mod.rs`
- `crates/agent_config/src/generator/default_config.rs`

**核心功能**:
```
DefaultConfigGenerator:
  - fn generate_claude_code_acp_config() -> AgentServersConfig
  - fn get_default_system_prompt_template() -> String
  - fn get_default_user_prompt_template() -> String
  - fn generate_default_mcp_servers() -> HashMap<String, ContextServerConfig>
  - async fn save_default_config(path: &Path) -> Result<()>
  - async fn ensure_default_config(config_path: &Path) -> Result<AgentServersConfig>
```

**验收标准**:
- [ ] 生成的配置与现有硬编码行为一致
- [ ] 默认 MCP 服务器包含 fetch 和 context7
- [ ] 系统提示词模板完整（从现有 system_prompt.rs 迁移）
- [ ] 配置文件不存在时自动生成

**设计文档参考**: 7.1.2 默认配置自动生成机制, L3026-3309

---

### P1-5: 实现 AgentConfigManager 配置管理器

**描述**: 整合配置解析和变量替换的统一管理器

**输出产物**:
- `crates/agent_config/src/manager.rs`

**核心功能**:
```
AgentConfigManager:
  - fn new(config: AgentServersConfig) -> Self
  - fn get_agent_config(&self, agent_id: &str) -> Option<&AgentServerConfig>
  - fn get_enabled_agents(&self) -> Vec<&AgentServerConfig>
  - fn resolve_agent_config(&self, agent_id: &str, model_provider: &ModelProviderConfig, project_context: &ProjectContext) -> Result<ResolvedAgentConfig>
  - async fn get_enabled_mcp_servers(&self) -> Result<Vec<String>>
  - fn get_mcp_server_config(&self, server_name: &str) -> Result<McpServerConfig>
  - async fn validate_enabled_mcp_servers(&self, model_provider: &ModelProviderConfig) -> Result<BatchValidationResult>
```

**验收标准**:
- [ ] 统一的配置获取入口
- [ ] 支持配置热重载（可选）
- [ ] 集成测试覆盖完整流程

**设计文档参考**: L427-455, L763-769

---

## P2: Agent 抽象层核心

### P2-1: 创建 `agent_abstraction` crate 基础结构

**描述**: 创建 Agent 抽象层的独立 crate

**输出产物**:
- `crates/agent_abstraction/Cargo.toml`
- `crates/agent_abstraction/src/lib.rs`

**依赖**:
- `agent_config`
- `shared_types`
- `tokio`
- `async-trait`
- `thiserror`

**验收标准**:
- [ ] crate 可以被 workspace 正确引用
- [ ] 正确配置 `async-trait` 支持 `?Send`

**设计文档参考**: 4.1 整体架构

---

### P2-2: 定义 Agent Trait 核心接口

**描述**: 定义 Agent 的核心抽象接口

**输出产物**:
- `crates/agent_abstraction/src/traits/mod.rs`
- `crates/agent_abstraction/src/traits/agent.rs`

**核心接口定义**:
```rust
#[async_trait::async_trait(?Send)]
pub trait Agent: Send + Sync {
    fn agent_type(&self) -> AgentType;
    
    async fn start(
        &self,
        config: AgentConfig,
        context: AgentContext,
    ) -> Result<AgentInstance, AgentError>;
    
    async fn stop(&self, instance: &AgentInstance) -> Result<(), AgentError>;
    
    async fn restart(
        &self,
        instance: &AgentInstance,
        config: AgentConfig,
        context: AgentContext,
    ) -> Result<AgentInstance, AgentError>;
    
    fn get_config(&self, instance: &AgentInstance) -> Option<&AgentConfig>;
}
```

**验收标准**:
- [ ] Trait 方法签名与设计文档一致
- [ ] 支持 `?Send` 以兼容 ACP 的 LocalSet 要求
- [ ] 提供默认的 restart 实现（stop + start）

**设计文档参考**: 4.2.1 Agent 抽象 Trait, L156-184

---

### P2-3: 定义 AgentLauncher Trait

**描述**: 定义 Agent 进程启动器接口

**输出产物**:
- `crates/agent_abstraction/src/traits/launcher.rs`

**核心接口定义**:
```rust
#[async_trait::async_trait(?Send)]
pub trait AgentLauncher: Send + Sync {
    async fn launch(
        &self,
        spec: &AgentSpec,
        config: &AgentConfig,
        context: &AgentContext,
    ) -> Result<LaunchedAgent, AgentError>;
    
    async fn terminate(
        &self,
        agent: &LaunchedAgent,
        timeout: Duration,
    ) -> Result<TerminationResult, AgentError>;
    
    async fn check_status(&self, agent: &LaunchedAgent) -> Result<ProcessStatus, AgentError>;
}
```

**验收标准**:
- [ ] 接口支持超时控制
- [ ] 定义 LaunchedAgent、TerminationResult、ProcessStatus 结构
- [ ] 单元测试定义

**设计文档参考**: 4.2.2 Agent 启动器, L213-231

---

### P2-4: 定义 AgentInstance 和 AgentContext

**描述**: 定义 Agent 运行实例和上下文信息

**输出产物**:
- `crates/agent_abstraction/src/instance.rs`
- `crates/agent_abstraction/src/context.rs`

**核心结构定义**:
```
AgentInstance:
  - config: AgentConfig
  - process: Option<AgentProcess>
  - status: AgentStatus
  - started_at: Option<DateTime<Utc>>

AgentContext:
  - project_id: String
  - project_path: PathBuf
  - timestamp: DateTime<Utc>

AgentStatus:
  - Stopped
  - Starting
  - Running
  - Stopping
  - Error(String)
  - Unknown
```

**验收标准**:
- [ ] AgentInstance 支持状态转换方法
- [ ] AgentContext 提供 `new()` 构造方法
- [ ] 状态转换逻辑正确

**设计文档参考**: L875-891, L3595-3609

---

## P3: 进程管理和生命周期

### P3-1: 实现 AgentProcess 进程封装

**描述**: 封装 tokio::process::Child，提供统一的进程管理接口

**输出产物**:
- `crates/agent_abstraction/src/process/mod.rs`
- `crates/agent_abstraction/src/process/agent_process.rs`

**核心结构定义**:
```
AgentProcess:
  - id: String
  - child: tokio::process::Child
  - config: AgentConfig
  - start_time: DateTime<Utc>

方法:
  - fn new(child: Child, config: AgentConfig) -> Self
  - fn id(&self) -> &str
  - async fn wait(&mut self) -> Result<ExitStatus>
  - async fn kill(&mut self) -> Result<()>
  - fn try_wait(&mut self) -> Result<Option<ExitStatus>>
  - fn stdin(&mut self) -> Option<&mut ChildStdin>
  - fn stdout(&mut self) -> Option<&mut ChildStdout>
```

**验收标准**:
- [ ] 正确封装 Child 的所有必要方法
- [ ] 支持获取 stdin/stdout 用于 ACP 通信
- [ ] 实现 Drop 时自动 kill

**设计文档参考**: L1279-1284

---

### P3-2: 实现 SubprocessLauncher 进程启动器

**描述**: 实现基于子进程的 Agent 启动器

**输出产物**:
- `crates/agent_abstraction/src/launcher/mod.rs`
- `crates/agent_abstraction/src/launcher/subprocess.rs`

**核心功能**:
```
SubprocessLauncher:
  - process_pool: Arc<ProcessPool>
  - monitor: Arc<ProcessMonitor>

方法:
  - fn new() -> Self
  - fn build_command(&self, spec: &AgentSpec, config: &AgentConfig, context: &AgentContext) -> Result<Command>
  - fn setup_environment(&self, cmd: &mut Command, spec: &AgentSpec, config: &AgentConfig) -> Result<()>

impl AgentLauncher:
  - 构建 Command
  - 设置环境变量
  - 配置 stdin/stdout/stderr
  - 启动进程
  - 创建 AgentProcess
```

**验收标准**:
- [ ] 正确合并环境变量（spec.env + config.env_overrides）
- [ ] 设置 kill_on_drop(true)
- [ ] 支持工作目录设置
- [ ] 单元测试（使用 mock 进程）

**设计文档参考**: 4.5.1 进程启动器实现, L2739-2801

---

### P3-3: 实现 AgentLifecycleManager 生命周期管理

**描述**: 管理 Agent 的完整生命周期和状态

**输出产物**:
- `crates/agent_abstraction/src/lifecycle/mod.rs`
- `crates/agent_abstraction/src/lifecycle/manager.rs`

**核心功能**:
```
AgentLifecycleManager:
  - processes: DashMap<String, AgentProcess>
  - agent_status_map: DashMap<String, AgentStatusInfo>

方法:
  - async fn start_agent(&self, config: &AgentConfig, context: &AgentContext) -> Result<AgentProcess>
  - async fn stop_agent(&self, agent_id: &str) -> Result<()>
  - async fn restart_agent(&self, agent_id: &str) -> Result<AgentProcess>
  - fn get_process_status(&self, agent_id: &str) -> Option<ProcessStatus>
  - fn is_agent_idle(&self, agent_id: &str) -> Option<bool>
  - fn get_agent_idle_status(&self, agent_id: &str) -> Option<AgentIdleStatus>
  - fn set_agent_active(&self, agent_id: &str, request_id: Option<String>)
  - fn set_agent_idle(&self, agent_id: &str)

AgentStatusInfo:
  - status: AgentStatus
  - session_id: Option<String>
  - request_id: Option<String>
  - last_activity: DateTime<Utc>
  - created_at: DateTime<Utc>

AgentIdleStatus:
  - is_idle: bool
  - current_status: AgentStatus
  - last_activity: DateTime<Utc>
  - session_id: Option<String>
  - current_request_id: Option<String>
  - idle_duration: Duration
```

**验收标准**:
- [ ] 使用 DashMap 保证线程安全
- [ ] 状态更新使用原子操作或最小化锁范围
- [ ] 支持空闲状态查询
- [ ] 集成测试

**设计文档参考**: L1193-1276

---

### P3-4: 实现 ProcessMonitor 进程监控（可选）

**描述**: 监控 Agent 进程的健康状态

**输出产物**:
- `crates/agent_abstraction/src/monitor/mod.rs`
- `crates/agent_abstraction/src/monitor/process_monitor.rs`

**核心功能**:
```
ProcessMonitor:
  - monitored_processes: DashMap<String, MonitoredProcess>
  - health_checker: Arc<HealthChecker>

方法:
  - async fn start_monitoring(&self, process: &ProcessHandle) -> Result<()>
  - async fn stop_monitoring(&self, process: &ProcessHandle) -> Result<()>
  - fn get_health_status(&self, process_id: &str) -> Option<HealthStatus>

HealthStatus:
  - Healthy
  - Unhealthy(String)
  - Dead
  - Unknown
```

**验收标准**:
- [ ] 后台定时健康检查
- [ ] 检测进程异常退出
- [ ] 可配置检查间隔

**设计文档参考**: 4.5.2 进程监控, L2804-2866

**优先级**: 低（MVP 可跳过）

---

## P4: ACP 连接池管理

### P4-1: 实现 AcpConnectionConfig 配置

**描述**: 定义 ACP 连接池的配置参数

**输出产物**:
- `crates/agent_abstraction/src/acp/mod.rs`
- `crates/agent_abstraction/src/acp/config.rs`

**核心结构定义**:
```
AcpConnectionConfig:
  - max_idle_time: Duration (default: 300s)
  - cleanup_interval: Duration (default: 60s)
  - connection_timeout: Duration (default: 30s)
  - max_connections: usize (default: 100)

impl Default for AcpConnectionConfig
```

**验收标准**:
- [ ] 提供合理的默认值
- [ ] 支持从环境变量覆盖配置
- [ ] 单元测试

**设计文档参考**: L1803-1826

---

### P4-2: 实现 AgentConnection 连接包装器

**描述**: 封装单个 ACP 连接，提供线程安全的访问

**输出产物**:
- `crates/agent_abstraction/src/acp/connection.rs`

**核心结构定义**:
```
AgentConnection:
  - agent_id: String
  - local_set: Box<LocalSet>
  - client_conn: RefCell<Option<ClientSideConnection>>
  - lifecycle_guard: AgentLifecycleGuard
  - last_activity: AtomicInstant
  - created_at: Instant
  - status: AtomicU8  // ConnectionStatus
  - manager_weak: Weak<AcpConnectionManager>

ConnectionStatus (repr u8):
  - Connecting = 1
  - Connected = 2
  - Idle = 3
  - Error = 4
  - Closed = 5

AtomicInstant:
  - inner: AtomicU64

方法:
  - fn get_status(&self) -> ConnectionStatus
  - fn set_status(&self, status: ConnectionStatus)
  - fn update_last_activity(&self)
  - fn idle_duration(&self) -> Duration
  - fn is_active(&self) -> bool
  - async fn execute_operation<F, R>(&self, operation: F) -> Result<R>

impl Drop for AgentConnection  // 自动从管理器移除
```

**验收标准**:
- [ ] 使用原子操作避免死锁
- [ ] 支持在 LocalSet 中执行操作
- [ ] Drop 时自动清理
- [ ] 单元测试

**设计文档参考**: L1831-1900, L2226-2297

---

### P4-3: 实现 AcpConnectionManager 连接池管理器

**描述**: 管理多个 ACP 连接的连接池

**输出产物**:
- `crates/agent_abstraction/src/acp/manager.rs`

**核心功能**:
```
AcpConnectionManager:
  - connections: Arc<DashMap<String, Weak<AgentConnection>>>
  - config: Arc<AcpConnectionConfig>
  - cleanup_task: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>

方法:
  - fn new(config: AcpConnectionConfig) -> Self
  - async fn get_or_create_connection(...) -> Result<Arc<AgentConnection>>
  - async fn create_new_connection(...) -> Result<Arc<AgentConnection>>
  - async fn send_prompt(&self, agent_id: &str, prompt_request: PromptRequest) -> Result<PromptResponse>
  - async fn cancel_request(&self, agent_id: &str, cancel_notification: CancelNotification) -> Result<()>
  - fn get_connection_stats(&self) -> ConnectionStats
  - fn start_cleanup_task(&self)

impl Drop for AcpConnectionManager  // 清理后台任务

ConnectionStats:
  - total_connections: usize
  - max_connections: usize
  - cleanup_interval: Duration
  - max_idle_time: Duration

AcpError:
  - ConnectionLimitExceeded
  - ConnectionNotAvailable
  - ProcessError(String)
  - ConnectionTimeout
  - ProtocolError(String)
  - ConfigurationError(String)
  - IoError(std::io::Error)
```

**验收标准**:
- [ ] 使用 Weak 引用避免循环依赖
- [ ] 后台清理任务正确启动和停止
- [ ] 连接复用逻辑正确
- [ ] 不会发生死锁
- [ ] 集成测试

**设计文档参考**: 4.4.5 ACP 连接池管理, L1766-2316

---

## P5: MCP 服务器管理

### P5-1: 实现 McpServerInstance 服务器实例

**描述**: 封装单个 MCP 服务器实例

**输出产物**:
- `crates/agent_abstraction/src/mcp/mod.rs`
- `crates/agent_abstraction/src/mcp/instance.rs`

**核心结构定义**:
```
McpServerInstance:
  - name: String
  - config: McpServerConfig
  - process: Option<ProcessHandle>
  - connection: Option<McpConnection>
  - started_at: Option<DateTime<Utc>>

ProcessHandle:
  - id: String
  - child: Child

McpConnection:
  - 基于 rmcp 库的连接封装
```

**验收标准**:
- [ ] 正确管理进程句柄
- [ ] 支持连接状态查询
- [ ] 单元测试

**设计文档参考**: L2408-2419

---

### P5-2: 实现 McpServerManager 服务器管理器

**描述**: 管理多个 MCP 服务器的启动、停止和状态

**输出产物**:
- `crates/agent_abstraction/src/mcp/manager.rs`

**核心功能**:
```
McpServerManager:
  - servers: DashMap<String, McpServerInstance>
  - config: Arc<McpConfig>
  - process_pool: Arc<McpProcessPool>

方法:
  - fn new(config: McpConfig) -> Self
  - async fn start_server(&self, server_name: &str, context: &AgentContext) -> Result<McpServerInstance>
  - async fn stop_server(&self, server_name: &str) -> Result<()>
  - async fn start_servers_for_agent(&self, server_names: &[String], context: &AgentContext) -> Result<Vec<McpServerInstance>>
  - fn get_server_status(&self, server_name: &str) -> Option<McpServerInstance>
  - fn list_servers(&self) -> Vec<String>
  - async fn start_command_server(&self, config: &McpServerConfig, context: &AgentContext) -> Result<ProcessHandle>
  - async fn establish_mcp_connection(&self, config: &McpServerConfig, process: &ProcessHandle) -> Result<McpConnection>
```

**验收标准**:
- [ ] 支持 Custom 和 Local 两种服务器类型
- [ ] 正确处理环境变量模板替换
- [ ] 连接建立超时处理
- [ ] 集成测试

**设计文档参考**: 4.4.6 MCP 服务器管理器, L2393-2593

---

### P5-3: 实现 McpServerValidator 验证器（可选）

**描述**: 验证 MCP 服务器配置的有效性

**输出产物**:
- `crates/agent_abstraction/src/mcp/validator.rs`

**核心功能**:
```
McpServerValidator:
  - default_timeout: Duration
  - working_dir: Option<PathBuf>

方法:
  - async fn validate_server(&self, config: &McpValidationConfig) -> Result<McpValidationResult>
  - async fn validate_batch(&self, configs: &[McpValidationConfig]) -> Result<BatchValidationResult>
  - async fn validate_from_json(&self, server_name: &str, json_config: &ContextServerConfig, model_provider: &ModelProviderConfig) -> Result<McpValidationResult>

McpValidationResult:
  - server_name: String
  - status: ValidationStatus
  - tools: Vec<McpToolInfo>
  - duration_ms: u64
  - error_message: Option<String>

BatchValidationResult:
  - total_servers: usize
  - enabled_servers: usize
  - skipped_servers: usize
  - success_count: usize
  - failed_count: usize
  - results: Vec<McpValidationResult>
```

**验收标准**:
- [ ] 只验证 enabled: true 的服务器
- [ ] 调用 tool/list 验证功能
- [ ] 提供详细的验证报告
- [ ] 集成测试

**设计文档参考**: 4.4.3 MCP 服务器配置验证库, L694-769

**优先级**: 低（MVP 可跳过）

---

## P6: Agent 工厂和注册表

### P6-1: 实现 AgentRegistry 注册表

**描述**: 管理 Agent 类型的注册和查找

**输出产物**:
- `crates/agent_abstraction/src/registry/mod.rs`
- `crates/agent_abstraction/src/registry/agent_registry.rs`

**核心功能**:
```
AgentRegistry:
  - agents: DashMap<AgentType, Arc<dyn Agent>>
  - specs: DashMap<AgentType, AgentSpec>

方法:
  - fn new() -> Self
  - fn register(&self, agent_type: AgentType, agent: Arc<dyn Agent>, spec: AgentSpec) -> Result<()>
  - fn get_implementation(&self, agent_type: &AgentType) -> Result<Arc<dyn Agent>>
  - fn get_spec(&self, agent_type: &AgentType) -> Result<AgentSpec>
  - fn list_agents(&self) -> Vec<AgentType>
  - fn unregister(&self, agent_type: &AgentType) -> Result<()>
```

**验收标准**:
- [ ] 防止重复注册
- [ ] 线程安全
- [ ] 单元测试

**设计文档参考**: 4.4.2 Agent 注册表, L2676-2730

---

### P6-2: 实现 AgentFactory 工厂

**描述**: 统一的 Agent 创建入口

**输出产物**:
- `crates/agent_abstraction/src/factory/mod.rs`
- `crates/agent_abstraction/src/factory/agent_factory.rs`

**核心功能**:
```
AgentFactory:
  - registry: Arc<AgentRegistry>
  - launcher: Arc<dyn AgentLauncher>
  - config_manager: Arc<AgentConfigManager>
  - mcp_manager: Arc<McpServerManager>
  - acp_connection_manager: Arc<AcpConnectionManager>

方法:
  - fn new(...) -> Self
  - async fn create_agent(&self, agent_type: AgentType, chat_prompt: ChatPrompt, model_provider: Option<ModelProviderConfig>) -> Result<AgentInstance>
  - async fn validate_dependencies(&self, spec: &AgentSpec) -> Result<()>
```

**验收标准**:
- [ ] 集成所有管理器组件
- [ ] 自动启动 MCP 服务器
- [ ] 正确传递配置和上下文
- [ ] 集成测试

**设计文档参考**: 4.5 Agent 工厂模式, L2596-2673

---

### P6-3: 实现 AgentManager 统一管理器

**描述**: 提供 Agent 管理的高层 API

**输出产物**:
- `crates/agent_abstraction/src/manager/mod.rs`
- `crates/agent_abstraction/src/manager/agent_manager.rs`

**核心功能**:
```
AgentManager:
  - config: AgentServersConfig
  - env_resolver: EnvironmentVariableResolver
  - lifecycle_manager: AgentLifecycleManager
  - installation_manager: AgentInstallationManager

方法:
  - fn new(config: AgentServersConfig, env_resolver: EnvironmentVariableResolver) -> Result<Self>
  - async fn start_agent(&mut self, agent_id: &str, project_id: &str, model_provider: &ModelProviderConfig) -> Result<AgentInstance>
  - async fn stop_agent(&mut self, agent_id: &str) -> Result<()>
  - fn get_agent_status(&self, agent_id: &str) -> Option<AgentStatus>
  - fn list_agents(&self) -> Vec<&AgentConfig>
  - fn list_enabled_agents(&self) -> Vec<&AgentConfig>
  - fn is_agent_idle(&self, project_id: &str) -> Option<bool>
  - fn get_agent_idle_status(&self, project_id: &str) -> Option<AgentIdleStatus>
  - fn list_idle_agents(&self) -> Vec<String>
  - fn get_idle_statistics(&self) -> AgentIdleStatistics
  - async fn validate_agent_config(&self, agent_config: &AgentConfig) -> Result<ValidationResult>
  - async fn install_agent(&self, agent_config: &AgentConfig) -> Result<()>
  - async fn update_agent(&self, agent_id: &str) -> Result<()>
```

**验收标准**:
- [ ] 统一的管理入口
- [ ] 支持空闲状态统计
- [ ] 集成测试

**设计文档参考**: 4.4.4 Agent 配置和管理模块, L835-995

---

## P7: 兼容层和迁移

### P7-1: 实现 ClaudeCodeAcpAgent 兼容层

**描述**: 保持现有 AcpAgentService 接口不变，内部使用新配置系统

**输出产物**:
- `crates/agent_runner/src/agent/claude_code_compat.rs`（新文件）

**核心功能**:
```
ClaudeCodeAcpAgent:
  - config_manager: Arc<AgentConfigManager>
  - acp_connection_manager: Arc<AcpConnectionManager>
  - default_agent_id: String

方法:
  - async fn new() -> Result<Self>  // 自动加载或生成默认配置
  - async fn start_claude_code_acp_agent_service(&self, chat_prompt: ChatPrompt, model_provider: Option<ModelProviderConfig>) -> Result<AcpConnectionInfo>

impl AcpAgentService for ClaudeCodeAcpAgent:
  - async fn start_agent_service(...) -> Result<AcpConnectionInfo>  // 调用内部新实现
  - fn agent_type_name(&self) -> &'static str  // 返回 "claude-code-acp"
```

**验收标准**:
- [ ] 现有 HTTP API 调用完全兼容
- [ ] 现有功能 100% 保持
- [ ] 自动生成默认配置文件
- [ ] 集成测试验证兼容性

**设计文档参考**: 7.1.3 兼容层实现, L3433-3511

---

### P7-2: 迁移现有 claude_code_agent.rs

**描述**: 将现有实现重构为使用新配置系统

**输出产物**:
- 修改 `crates/agent_runner/src/agent/claude_code_agent.rs`

**迁移步骤**:
1. 保留现有函数签名
2. 内部调用 ClaudeCodeAcpAgent
3. 移除硬编码的 command、args、env
4. 使用 AgentConfigManager 获取配置
5. 使用 EnvironmentVariableResolver 解析变量

**验收标准**:
- [ ] 所有现有测试通过
- [ ] 行为与迁移前一致
- [ ] 代码量减少（配置外移）

**设计文档参考**: 7.1.4 迁移执行策略, L3488-3529

---

### P7-3: 迁移 system_prompt.rs 到配置文件

**描述**: 将硬编码的系统提示词迁移到配置文件

**输出产物**:
- 修改 `crates/agent_runner/src/prompt/system_prompt.rs`
- 默认配置文件中的 `system_prompt.template`

**迁移步骤**:
1. 将现有提示词内容提取到 DefaultConfigGenerator
2. 修改 system_prompt.rs 从配置读取
3. 保留变量替换逻辑
4. 添加 enabled 开关支持

**验收标准**:
- [ ] 默认提示词与现有完全一致
- [ ] 支持用户自定义提示词
- [ ] 支持禁用系统提示词

**设计文档参考**: L3041-3309

---

## P8: 集成和测试

### P8-1: 编写单元测试

**描述**: 为所有新模块编写单元测试

**输出产物**:
- `crates/agent_config/src/tests/`
- `crates/agent_abstraction/src/tests/`

**测试覆盖**:
- [ ] 配置解析和验证
- [ ] 环境变量替换
- [ ] 提示词模板渲染
- [ ] 状态转换逻辑
- [ ] 错误处理

**验收标准**:
- [ ] 代码覆盖率 > 80%
- [ ] 所有边界条件测试
- [ ] CI 通过

---

### P8-2: 编写集成测试

**描述**: 端到端测试验证完整流程

**输出产物**:
- `tests/integration/agent_abstraction_test.rs`

**测试场景**:
- [ ] 加载配置 -> 解析变量 -> 启动 Agent -> 发送提示词 -> 停止 Agent
- [ ] 多 Agent 并发启动
- [ ] 配置热重载（如支持）
- [ ] 错误恢复

**验收标准**:
- [ ] 覆盖主要使用场景
- [ ] CI 通过

---

### P8-3: 更新文档和示例

**描述**: 更新 README 和添加使用示例

**输出产物**:
- 更新 `README.md`
- `examples/custom_agent.rs`
- `examples/config/agents.json`

**内容**:
- [ ] 配置文件格式说明
- [ ] 自定义 Agent 示例
- [ ] MCP 服务器配置示例
- [ ] 迁移指南

**验收标准**:
- [ ] 文档清晰完整
- [ ] 示例可运行

---

## 任务依赖关系图

```
P0-1 ─┬─> P0-2 ─┬─> P1-1 ─> P1-2 ─> P1-3 ─> P1-4 ─> P1-5
      │         │
      │         └─> P0-3 ─┘
      │
      └─> P0-4 ─┘

P0 完成后:
      
P2-1 ─> P2-2 ─> P2-3 ─> P2-4
          │
          └─────────────────────┐
                                │
P3-1 ─> P3-2 ─> P3-3 ─> P3-4    │
          │                     │
          └─────────────────────┤
                                │
P4-1 ─> P4-2 ─> P4-3 ──────────┤
                                │
P5-1 ─> P5-2 ─> P5-3 ──────────┤
                                │
                                v
                    P6-1 ─> P6-2 ─> P6-3
                                │
                                v
                    P7-1 ─> P7-2 ─> P7-3
                                │
                                v
                    P8-1 ─> P8-2 ─> P8-3
```

---

## 开发注意事项

### 1. ACP 协议约束
- `ClientSideConnection` 和 `AgentSideConnection` **未实现 Send trait**
- 必须在 `LocalSet` 和 `spawn_local` 中使用
- 参考: `agent-client-protocol/rust/examples`

### 2. 并发安全
- 使用 `DashMap` 替代 `Arc<RwLock<HashMap>>`
- 状态更新使用原子操作
- 避免嵌套锁导致死锁

### 3. 错误处理
- 使用 `thiserror` 定义错误类型
- 错误信息使用中文
- 保留完整的错误链

### 4. 配置优先级
1. 命令行参数
2. 环境变量
3. 配置文件
4. 代码默认值

### 5. 向后兼容
- 保持现有 HTTP API 不变
- 保持 `AcpAgentService` trait 签名不变
- 默认配置与硬编码行为一致

---

## 版本规划

| 版本 | 包含任务 | 目标 |
|------|----------|------|
| v0.1.0 | P0, P1, P2 | 基础类型和配置系统 |
| v0.2.0 | P3, P4 | 进程管理和 ACP 连接池 |
| v0.3.0 | P5, P6 | MCP 管理和 Agent 工厂 |
| v1.0.0 | P7, P8 | 兼容层和完整测试 |

---

> 最后更新: 2024-12-03
> 设计文档: specs/agent-abstraction-layer-design.md
