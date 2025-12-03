# Agent 抽象层设计方案

## 1. 概述

本设计旨在解决 RCoder 项目中 **Agent、MCP 服务器和提示词硬编码**的核心问题，通过构建独立的配置化模块实现动态扩展能力。

### 🎯 核心目标
- **Agent 动态配置**：支持多种 AI Agent 类型的插件化管理和运行
- **MCP 服务动态配置**：支持 MCP 服务器的热插拔和统一管理  
- **提示词模板化**：将系统提示词和用户提示词包装逻辑配置化
- **模块化架构**：Agent 和 MCP 以独立 library 形式提供，供 rcoder、agent_runner 等模块使用

### 📦 架构设计原则
1. **配置驱动**：所有 Agent、MCP、提示词通过 JSON 配置文件定义
2. **模块解耦**：Agent 抽象层和 MCP 管理器作为独立的 crate 库
3. **向后兼容**：现有硬编码的 claude-code-acp 实现平滑迁移到新配置系统
4. **零停机更新**：支持运行时动态加载新的 Agent 和 MCP 服务

### 🏗️ 模块分层
```
┌─────────────────────────────────────────┐
│           RCoder 主应用                  │
├─────────────────────────────────────────┤
│         Agent Runner 模块               │
├─────────────────────────────────────────┤
│  Agent 抽象层 lib  │ MCP 管理器 lib      │
│  (多Agent支持)      │ (工具服务管理)      │
├─────────────────────────────────────────┤
│         配置管理系统                     │
│    (Agent/MCP/提示词配置)                │
└─────────────────────────────────────────┘
```

### 💡 解决的痛点
- ❌ **硬编码问题**：Agent 类型、MCP 服务、提示词写死在代码中
- ❌ **扩展困难**：添加新 Agent 需要修改核心代码
- ❌ **配置僵化**：无法根据项目需求动态调整 Agent 行为
- ❌ **维护成本**：每次提示词或配置变更都需要重新编译部署

## 2. 现状分析

### 2.1 当前实现分析

基于 `claude_code_agent.rs` 的分析，当前 Agent 实现具有以下特征：

**核心组件：**
- **进程管理**：通过 `tokio::process::Command` 启动子进程
- **ACP 协议集成**：使用 `ClientSideConnection` 与 Agent 通信
- **环境变量配置**：通过 `merged_envs` 传递模型配置
- **生命周期管理**：使用 `CancellationToken` 和 `AgentLifecycleGuard`
- **错误处理**：统一的错误传播机制

**关键流程：**
```rust
// 1. 启动子进程
let mut child = tokio::process::Command::new("claude-code-acp")
    .args(&spawn_args)
    .envs(merged_envs)
    .spawn()?;

// 2. 建立 ACP 连接
let (client_conn, handle_io) = ClientSideConnection::new(client, outgoing, incoming, |fut| {
    tokio::task::spawn_local(fut);
});

// 3. 初始化和会话管理
client_conn.initialize(init_request).await?;
let session_id = client_conn.new_session(session_request).await?;

// 4. 消息处理
super::channel_utils::spawn_prompt_handler_for_agent(/*...*/);
super::channel_utils::spawn_cancel_handler_for_agent(/*...*/);
```

### 2.2 现有抽象层

当前已具备基础的抽象：

**AcpAgentService Trait：**
```rust
#[async_trait::async_trait(?Send)]
pub trait AcpAgentService {
    async fn start_agent_service(
        &self,
        chat_prompt: ChatPrompt,
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<AcpConnectionInfo>;
    
    fn agent_type_name(&self) -> &'static str;
}
```

**AgentType 枚举：**
```rust
pub enum AgentType {
    Claude,
    #[cfg(feature = "codex")]
    Codex,
}
```

**生命周期管理：**
- `AgentLifecycleGuard` - RAII 风格的资源管理
- `AgentLifecycle` trait - 统一的生命周期接口

## 3. 设计目标

### 3.1 核心目标

1. **可扩展性**：支持新的 Agent 类型无需修改现有代码
2. **配置灵活性**：支持通过配置文件和环境变量管理 Agent
3. **进程隔离**：每个 Agent 运行在独立进程中，确保稳定性
4. **统一接口**：所有 Agent 使用相同的 ACP 协议接口
5. **资源管理**：统一的资源清理和错误处理机制

### 3.2 非功能性目标

- **性能**：最小化抽象层开销
- **可观测性**：统一的日志和监控指标
- **安全性**：隔离 Agent 进程，限制权限
- **可测试性**：支持单元测试和集成测试

## 4. 架构设计

### 4.1 整体架构

```
┌─────────────────────────────────────────────────────────────┐
│                    RCoder Core                              │
├─────────────────────────────────────────────────────────────┤
│                Agent Manager                                │
│  ┌─────────────────┐  ┌─────────────────┐  ┌──────────────┐ │
│  │  Agent Factory │  │ Config Manager  │  │  Registry    │ │
│  └─────────────────┘  └─────────────────┘  └──────────────┘ │
├─────────────────────────────────────────────────────────────┤
│                Agent Abstraction Layer                      │
│  ┌─────────────────┐  ┌─────────────────┐  ┌──────────────┐ │
│  │  Agent Trait    │  │  Launcher       │  │  Supervisor  │ │
│  └─────────────────┘  └─────────────────┘  └──────────────┘ │
├─────────────────────────────────────────────────────────────┤
│                Concrete Agent Implementations               │
│  ┌─────────────────┐  ┌─────────────────┐  ┌──────────────┐ │
│  │ Claude Code     │  │  Custom Agent   │  │  Future      │ │
│  │ ACP Agent       │  │  Implementation │  │  Agents      │ │
│  └─────────────────┘  └─────────────────┘  └──────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

### 4.2 核心组件设计

#### 4.2.1 Agent 抽象 Trait

```rust
/// Agent 抽象接口
#[async_trait::async_trait(?Send)]
pub trait Agent: Send + Sync {
    /// Agent 类型标识
    fn agent_type(&self) -> AgentType;
    
    /// 启动 Agent 服务
    /// 
    /// 使用 Command 方式启动 Agent 进程，并建立 ACP 连接
    async fn start(
        &self,
        config: AgentConfig,
        context: AgentContext,
    ) -> Result<AgentInstance, AgentError>;
    
    /// 停止 Agent 服务
    async fn stop(&self, instance: &AgentInstance) -> Result<(), AgentError>;
    
    /// 重启 Agent 服务
    /// 
    /// 先停止当前实例，然后重新启动
    async fn restart(
        &self,
        instance: &AgentInstance,
        config: AgentConfig,
        context: AgentContext,
    ) -> Result<AgentInstance, AgentError>;
    
    /// 获取当前 Agent 使用的配置
    fn get_config(&self, instance: &AgentInstance) -> Option<&AgentConfig>;
}

/// Agent 错误类型
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("启动失败: {0}")]
    StartupFailed(String),
    
    #[error("进程错误: {0}")]
    ProcessError(String),
    
    #[error("配置错误: {0}")]
    ConfigurationError(String),
    
    #[error("连接错误: {0}")]
    ConnectionError(String),
    
    #[error("IO错误: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("其他错误: {0}")]
    Other(String),
}

#### 4.2.2 Agent 启动器

```rust
/// Agent 启动器抽象
#[async_trait::async_trait(?Send)]
pub trait AgentLauncher: Send + Sync {
    /// 启动 Agent 进程
    async fn launch(
        &self,
        spec: &AgentSpec,
        config: &AgentConfig,
        context: &AgentContext,
    ) -> Result<LaunchedAgent, AgentError>;
    
    /// 停止 Agent 进程
    async fn terminate(
        &self,
        agent: &LaunchedAgent,
        timeout: Duration,
    ) -> Result<TerminationResult, AgentError>;
    
    /// 检查进程状态
    async fn check_status(&self, agent: &LaunchedAgent) -> Result<ProcessStatus, AgentError>;
}
```

#### 4.2.3 Agent 规范定义

```rust
/// Agent 规范定义
/// 
/// 基于实际 JSON 配置格式的 Agent 规范结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Agent 唯一标识（与 JSON 配置中的 key 对应）
    pub agent_id: String,
    
    /// Agent 类型
    pub agent_type: AgentType,
    
    /// 启动命令
    pub command: String,
    
    /// 命令参数
    pub args: Vec<String>,
    
    /// 环境变量配置
    pub env: HashMap<String, String>,
    
    /// 安装配置
    pub installation: InstallationConfig,
    
    /// 🔥 新增：系统提示词配置
    pub system_prompt: Option<SystemPromptConfig>,
    
    /// 🔥 新增：用户提示词包装配置
    pub user_prompt: Option<UserPromptConfig>,
    
    /// 是否启用
    pub enabled: bool,
    
    /// 元数据信息
    pub metadata: HashMap<String, String>,
}

/// 🔥 系统提示词配置
/// 
/// 简洁设计：包含一个模板字段，支持动态变量替换
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPromptConfig {
    /// 系统提示词模板内容
    /// 预处理好的完整提示词，支持变量替换（如 {PROJECT_NAME}、{FRAMEWORK} 等）
    pub template: String,
    
    /// 是否启用系统提示词（默认为 true）
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// 默认启用状态为 true
fn default_enabled() -> bool {
    true
}

/// 🔥 新增：用户提示词包装配置
/// 
/// 用于包装用户的实际输入内容，支持模板变量 {user_prompt}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPromptConfig {
    /// 用户提示词模板，其中 {user_prompt} 会被用户的实际输入替换
    pub template: String,
    
    /// 是否启用用户提示词包装（默认为 true）
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// 与 JSON 配置的对应关系：
/// 
/// ```json
/// {
///   "agent_servers": {
///     "claude-code-acp": {           // ← agent_id
///       "agent_type": "claude",      // ← agent_type  
///       "command": "claude-code-acp", // ← command
///       "args": [],                  // ← args
///       "env": { ... },             // ← env
///       "system_prompt": { ... },    // ← system_prompt (系统提示词配置)
///       "user_prompt": { ... },      // ← user_prompt (用户提示词包装配置)
///       "installation": { ... },    // ← installation
///       "enabled": true,             // ← enabled
///       "metadata": { ... }          // ← metadata
///     }
///   }
/// }
/// 
/// system_prompt 示例：
/// {
///   "system_prompt": {
///     "main_prompt": "你是一个专业的 React 开发助手",
///     "domain_prompt": "专注于 React.js 和现代前端技术",
///     "style_prompt": "代码现代、简洁、注重性能",
///     "code_standards": "使用函数组件、React Hooks、TypeScript",
///     "custom_prompts": ["优先使用最佳实践", "注重用户体验"]
///   }
/// }
/// ```
```


### 4.3 配置管理系统

**🔥 系统提示词使用示例：**

```json
{
  "agent_servers": {
    "react-developer": {
      "agent_type": "claude",
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
        "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}"
      },
      "system_prompt": {
        "template": "你是一个专业的 React 开发助手，专注于现代前端开发。使用函数组件、React Hooks、TypeScript，注重代码性能和用户体验。遵循现代最佳实践，保持代码简洁和可维护性。"
      },
      "installation": {
        "package_manager": "npm",
        "package_name": "@zed-industries/claude-code-acp",
        "version": "latest"
      },
      "enabled": true
    },
    "rust-expert": {
      "agent_type": "claude",
      "command": "claude-code-acp", 
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
        "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}"
      },
      "system_prompt": {
        "template": "你是一个 Rust 专家，精通系统编程、内存安全和并发编程。编写安全、高效的 Rust 代码，遵循 Rust 最佳实践，使用 Result 处理错误，避免不必要的 unsafe 代码。注重代码的类型安全和性能优化。"
      },
      "installation": {
        "package_manager": "npm",
        "package_name": "@zed-industries/claude-code-acp", 
        "version": "latest"
      },
      "enabled": true
    },
    "data-analyst": {
      "agent_type": "claude",
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
        "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}"
      },
      "system_prompt": {
        "template": "你是一个数据分析师，擅长数据处理、统计分析和数据可视化。使用 Python/pandas/R 进行数据分析，注重统计准确性和数据可视化效果，保护用户隐私和数据安全。"
      },
      "installation": {
        "package_manager": "npm",
        "package_name": "@zed-industries/claude-code-acp",
        "version": "latest"
      },
      "enabled": true
    }
  }
}
```

#### 4.3.0 环境变量映射系统

**设计理念：**
为了实现 Agent 配置的标准化和灵活性，我们设计了环境变量映射系统：

- **Key（环境变量名）**：保持每个 Agent 需要的特定环境变量名（如 `ANTHROPIC_API_KEY`、`KIMI_API_KEY`）
- **Value（变量引用）**：使用标准化的 ModelProviderConfig 字段引用（如 `{MODEL_PROVIDER_API_KEY}`）

**标准变量映射：**
```rust
// ModelProviderConfig 字段到环境变量的映射
pub struct ModelProviderEnvMapping {
    pub MODEL_PROVIDER_ID: String,           // 对应 ModelProviderConfig::id
    pub MODEL_PROVIDER_NAME: String,         // 对应 ModelProviderConfig::name
    pub MODEL_PROVIDER_BASE_URL: String,     // 对应 ModelProviderConfig::base_url
    pub MODEL_PROVIDER_API_KEY: String,      // 对应 ModelProviderConfig::api_key
    pub MODEL_PROVIDER_REQUIRES_OPENAI_AUTH: bool,  // 对应 ModelProviderConfig::requires_openai_auth
    pub MODEL_PROVIDER_DEFAULT_MODEL: String, // 对应 ModelProviderConfig::default_model
    pub MODEL_PROVIDER_API_PROTOCOL: Option<String>, // 对应 ModelProviderConfig::api_protocol
}
```

**运行时变量替换：**
```rust
impl AgentConfigManager {
    /// 解析环境变量中的占位符
    pub fn resolve_env_variables(
        &self, 
        env_config: &HashMap<String, String>,
        model_provider: &ModelProviderConfig
    ) -> HashMap<String, String> {
        let mut resolved_env = HashMap::new();
        
        for (key, value) in env_config {
            let resolved_value = self.replace_placeholders(value, model_provider);
            resolved_env.insert(key.clone(), resolved_value);
        }
        
        resolved_env
    }
    
    /// 替换占位符为实际值
    fn replace_placeholders(&self, template: &str, provider: &ModelProviderConfig) -> String {
        template
            .replace("{MODEL_PROVIDER_ID}", &provider.id)
            .replace("{MODEL_PROVIDER_NAME}", &provider.name)
            .replace("{MODEL_PROVIDER_BASE_URL}", &provider.base_url)
            .replace("{MODEL_PROVIDER_API_KEY}", &provider.api_key)
            .replace("{MODEL_PROVIDER_REQUIRES_OPENAI_AUTH}", &provider.requires_openai_auth.to_string())
            .replace("{MODEL_PROVIDER_DEFAULT_MODEL}", &provider.default_model)
            .replace("{MODEL_PROVIDER_API_PROTOCOL}", &provider.api_protocol.as_deref().unwrap_or(""))
    }
}
```

**使用示例：**
```json
{
  "env": {
    "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
    "ANTHROPIC_BASE_URL": "{MODEL_PROVIDER_BASE_URL}",
    "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}"
  }
}
```

这种设计允许：
1. **Agent 特定的环境变量名**：每个 Agent 可以使用自己期望的环境变量名
2. **统一的配置源**：所有 Agent 都从 ModelProviderConfig 获取配置值
3. **灵活的映射**：支持部分字段覆盖和自定义配置

#### 4.3.1 Agent 配置结构

```rust
/// Agent 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent 类型
    pub agent_type: AgentType,
    
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    
    /// 自定义参数
    pub custom_args: Vec<String>,
    
    /// 环境变量覆盖
    pub env_overrides: HashMap<String, String>,
    
    /// 系统提示词
    pub system_prompt: Option<String>,
    
    /// 🔥 新增：用户提示词包装配置
    pub user_prompt: Option<UserPromptConfig>,
    
    /// MCP 服务器配置
    pub mcp_servers: Vec<McpServerConfig>,
}

/// MCP 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// 服务器名称
    pub name: String,
    
    /// 服务器来源类型
    pub source: McpServerSource,
    
    /// 是否启用
    pub enabled: bool,
    
    /// 启动命令（对于 custom 类型的服务器）
    pub command: Option<String>,
    
    /// 命令参数
    pub args: Option<Vec<String>>,
    
    /// 环境变量
    pub env: Option<HashMap<String, String>>,
    
    /// 连接超时
    pub timeout: Option<Duration>,
}

/// MCP 服务器来源类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum McpServerSource {
    /// 自定义命令行工具（支持 npm、uvx、bun、cargo、python 等命令）
    /// 
    /// 示例：
    /// - `npx @modelcontextprotocol/server-fetch`
    /// - `uvx mcp-server-fetch` 
    /// - `bun @upstash/context7-mcp`
    /// - `cargo install --path ./mcp-server`
    Custom,
    /// 本地可执行文件（直接指定可执行文件路径）
    /// 
    /// 示例：
    /// - `/usr/local/bin/mcp-server`
    /// - `/opt/mcp/custom-tools`
    Local,
}
```

#### 4.3.2 标准化环境变量映射

**ModelProviderConfig 字段映射：**

基于当前项目中的 `ModelProviderConfig` 结构体，我们定义以下标准化环境变量映射：

```rust
// ModelProviderConfig 结构体字段：
pub struct ModelProviderConfig {
    pub id: String,                    // → {MODEL_PROVIDER_ID}
    pub name: String,                  // → {MODEL_PROVIDER_NAME}  
    pub base_url: String,              // → {MODEL_PROVIDER_BASE_URL}
    pub api_key: String,               // → {MODEL_PROVIDER_API_KEY}
    pub requires_openai_auth: bool,    // → {MODEL_PROVIDER_REQUIRES_OPENAI_AUTH}
    pub default_model: String,         // → {MODEL_PROVIDER_DEFAULT_MODEL}
    pub api_protocol: Option<String>,  // → {MODEL_PROVIDER_API_PROTOCOL}
}
```

**标准化环境变量定义：**

| 变量名 | 对应字段 | 描述 | 示例值 |
|--------|----------|------|--------|
| `{MODEL_PROVIDER_ID}` | `id` | 模型提供商唯一标识 | "anthropic-claude" |
| `{MODEL_PROVIDER_NAME}` | `name` | 模型提供商显示名称 | "Claude" |
| `{MODEL_PROVIDER_BASE_URL}` | `base_url` | API 基础URL | "https://api.anthropic.com" |
| `{MODEL_PROVIDER_API_KEY}` | `api_key` | API 密钥 | "sk-ant-xxx" |
| `{MODEL_PROVIDER_DEFAULT_MODEL}` | `default_model` | 默认模型 | "glm-4.6" |
| `{MODEL_PROVIDER_API_PROTOCOL}` | `api_protocol` | API 协议 | "openai" |

**配置文件格式：**

```json
{
  "agent_servers": {
    "claude-code-acp": {
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
        "ANTHROPIC_BASE_URL": "{MODEL_PROVIDER_BASE_URL}",
        "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}",
        "RUST_LOG": "info"
      },
      "system_prompt": {
        "template": "你是一个专业的全栈开发助手，擅长现代 Web 开发和系统架构。专注于 JavaScript/TypeScript、React、Node.js 等技术栈，代码要现代、可维护、注重性能优化和用户体验。遵循最佳实践，重视代码质量和团队协作。"
      },
      "user_prompt": {
        "template": "你是RCoder，一个专业的AI编程助手。\n\n## 核心身份与职责\n- 专业的编程助手，帮助用户解决编程问题\n- 提供简洁、实用、可执行的代码解决方案\n- 遵循最佳实践，编写高质量代码\n- 始终将用户需求放在首位\n\n## 代码格式规范\n- 优先使用现代语言特性和标准库\n- 变量和函数命名使用清晰、描述性的英文名称\n- 保持代码简洁，避免过度复杂的抽象\n- 使用适当的注释解释关键逻辑\n\n## 开发约束\n- 避免添加未请求的功能，保持解决方案专注\n- 优先选择最简单有效的实现方式\n- 不要为未来可能的需求添加复杂性\n- 确保代码安全、可维护\n\n## MCP工具使用指导\n- 合理使用可用的工具来辅助开发任务\n- 当需要文件操作、搜索、测试时使用相应的工具\n- 根据上下文选择最合适的工具\n\n## 思考要求\n- 在回答前进行充分的思考和分析\n- 确保解决方案的完整性和正确性\n- 提供清晰、有条理的回答\n\n用户请求：\n{user_prompt}"
      },
      "installation": {
        "package_manager": "npm",
        "package_name": "@zed-industries/claude-code-acp",
        "version": "latest",
        "validate_command": ["claude-code-acp", "--version"]
      }
    },
    "Kimi CLI": {
      "command": "kimi",
      "args": ["--acp"],
      "env": {
        "KIMI_API_KEY": "{MODEL_PROVIDER_API_KEY}",
        "KIMI_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}",
        "KIMI_BASE_URL": "{MODEL_PROVIDER_BASE_URL}",
        "KIMI_API_PROTOCOL": "{MODEL_PROVIDER_API_PROTOCOL}"
      },
      "system_prompt": {
        "template": "你是 Kimi AI 助手，专注于提供准确、有用的信息检索和问答服务。基于先进的语言模型，能够处理复杂查询，提供详细且相关的回答。保持客观中立，提供经过验证的信息。"
      },
      "user_prompt": {
        "template": "请帮我查询并回答以下问题：{user_prompt}"
      },
      "installation": {
        "package_manager": "npm",
        "package_name": "@kimi-ai/cli",
        "version": "^1.0.0"
      }
    },
    "custom-agent": {
      "command": "custom-agent",
      "args": ["--mode", "acp", "--project", "{project_id}"],
      "env": {
        "CUSTOM_API_KEY": "{CUSTOM_API_KEY}",
        "RUST_LOG": "debug"
      },
      "system_prompt": {
        "template": "你是一个自定义 AI 助手，根据项目需求提供专业的开发支持。请仔细分析用户的具体需求，提供针对性的帮助和建议。"
      },
      "user_prompt": {
        "enabled": false,
        "template": "{user_prompt}"
      },
      "installation": {
        "package_manager": "git",
        "source": "https://github.com/user/custom-agent.git",
        "version": "main"
      }
    }
  },
  "context_servers": {
    "fetch": {
      "source": "custom",
      "enabled": true,
      "command": "uvx",
      "args": ["mcp-server-fetch"],
      "env": {}
    },
    "context7": {
      "source": "custom",
      "enabled": true,
      "command": "npx",
      "args": ["-y", "@upstash/context7-mcp"],
      "env": {
        "CONTEXT7_API_KEY": "{CONTEXT7_API_KEY}"
      }
    },
    "custom-tools": {
      "source": "local",
      "enabled": true,
      "command": "/opt/mcp/custom-tools",
      "args": ["--config", "/etc/mcp/config.yml"],
      "env": {
        "CUSTOM_TOOLS_CONFIG": "/etc/mcp/config.yml"
      }
    },
    "web-search": {
      "source": "custom",
      "enabled": true,
      "command": "mcp-server-search",
      "args": ["--engine", "google"],
      "env": {
        "SEARCH_API_KEY": "{SEARCH_API_KEY}"
      }
    },
    "database": {
      "source": "custom",
      "enabled": true,
      "command": "mcp-server-database",
      "args": ["--connection", "{DATABASE_URL}"],
      "env": {
        "DATABASE_URL": "{DATABASE_URL}"
      }
    }
  }
}
```

#### 4.4.3 MCP 服务器配置验证库

**设计目标：**
创建基于 `rmcp` 库的独立验证模块（`crates/mcp_validator`），用于验证 `enabled: true` 的 MCP 服务器配置的有效性。

**核心功能：**
1. 筛选启用服务器：只验证配置中 `enabled: true` 的 MCP 服务器
2. 进程启动验证：启动 MCP 服务器进程并建立连接
3. 工具列表检查：通过 `tool/list` 接口验证服务器可用性
4. 批量验证：支持同时验证多个启用的服务器
5. 统计报告：提供详细的验证结果和统计信息

**验证策略：**
- **启用过滤**：自动跳过所有 `enabled: false` 的服务器配置
- **连接测试**：使用 rmcp 库建立与 MCP 服务器的连接
- **工具验证**：调用 `tool/list` 确认服务器能正常返回工具列表
- **错误处理**：提供详细的错误信息和验证状态

**核心结构设计：**

```rust
// crates/mcp_validator/src/lib.rs

/// 验证结果
pub struct McpValidationResult {
    pub server_name: String,
    pub status: ValidationStatus,      // Success/Failed/Timeout 等
    pub tools: Vec<McpToolInfo>,       // 可用工具列表
    pub duration_ms: u64,              // 验证耗时
    pub error_message: Option<String>, // 错误信息
}

/// 批量验证结果
pub struct BatchValidationResult {
    pub total_servers: usize,     // 总服务器数（包括禁用的）
    pub enabled_servers: usize,   // 启用的服务器数
    pub skipped_servers: usize,   // 跳过的禁用服务器数
    pub success_count: usize,     // 成功验证数
    pub failed_count: usize,      // 失败验证数
    pub results: Vec<McpValidationResult>, // 详细结果
}

/// MCP 服务器验证器
pub struct McpServerValidator {
    default_timeout: Duration,
    working_dir: Option<PathBuf>,
}

impl McpServerValidator {
    /// 验证单个 MCP 服务器配置
    pub async fn validate_server(&self, config: &McpValidationConfig) -> Result<McpValidationResult, McpValidationError>;
    
    /// 批量验证 MCP 服务器
    pub async fn validate_batch(&self, configs: &[McpValidationConfig]) -> Result<BatchValidationResult, McpValidationError>;
    
    /// 从 JSON 配置验证（自动处理 enabled 过滤）
    pub async fn validate_from_json(&self, server_name: &str, json_config: &ContextServerConfig, model_provider: &ModelProviderConfig) -> Result<McpValidationResult, McpValidationError>;
}

/// 便捷 API 函数
pub async fn validate_all_mcp_servers(
    context_servers: &HashMap<String, ContextServerConfig>,
    model_provider: &ModelProviderConfig,
) -> Result<BatchValidationResult, McpValidationError>;
```

**集成到配置管理：**

```rust
impl AgentConfigManager {
    /// 验证所有启用的 MCP 服务器配置
    pub async fn validate_enabled_mcp_servers(&self, model_provider: &ModelProviderConfig) -> Result<BatchValidationResult, ConfigError> {
        validate_all_mcp_servers(&self.config.context_servers, model_provider)
            .await.map_err(|e| ConfigError::ValidationError(e.to_string()))
    }
}
```

**验证流程：**
1. **配置解析**：从 JSON 配置中提取 MCP 服务器信息
2. **启用过滤**：只处理 `enabled: true` 的服务器
3. **环境变量替换**：使用 `{VARIABLE_NAME}` 格式进行变量映射
4. **进程启动**：启动 MCP 服务器子进程
5. **连接建立**：通过 rmcp 建立 MCP 连接
6. **工具验证**：调用 `tool/list` 接口验证功能
7. **结果收集**：统计验证结果和性能数据
8. **进程清理**：确保子进程被正确清理

**输出示例：**
```
Validation Summary:
  Total servers: 5
  Enabled servers: 3
  Skipped (disabled): 2
  Successful: 2
  Failed: 1
  Total duration: 1250ms

✓ context7: 8 tools (320ms)
✓ fetch: 3 tools (180ms)
✗ web-search: Connection timeout (750ms)
```

#### 4.4.4 Agent 配置和管理模块

**设计目标：**
创建独立的 Agent 配置和管理模块（`crates/agent_manager`），提供统一的 Agent 配置、生命周期管理和抽象接口，作为 lib 库供其他模块使用。

**核心功能：**
1. **配置管理**：统一管理各种 Agent 的配置和安装信息
2. **抽象接口**：提供统一的 Agent 抽象，支持不同类型的 Agent
3. **生命周期管理**：Agent 的启动、停止、状态监控
4. **安装管理**：支持从多种源（npm、git、本地）自动安装 Agent
5. **验证功能**：验证 Agent 配置和可用性
6. **环境变量映射**：统一的变量替换和配置解析

**模块职责：**
- **配置标准化**：定义统一的 Agent 配置格式
- **插件化架构**：支持动态添加新的 Agent 类型
- **依赖管理**：处理 Agent 的依赖关系和版本管理
- **资源隔离**：确保不同 Agent 之间的资源隔离
- **错误处理**：统一的错误处理和日志记录

**容器环境设计原则：**
- **无状态管理**：Agent 在容器中运行，一段时间无使用且无任务执行时自动销毁
- **安装验证**：确认 Agent 可以成功安装即可，不需要卸载和列表管理
- **轻量化**：避免复杂的状态管理，简化安装流程
- **自动清理**：依赖容器生命周期自动管理资源

**核心功能优先原则：**
- **MVP 设计**：专注于核心功能实现，避免过度工程化
- **简单直接**：优先实现最基本但完整的功能
- **后续扩展**：性能监控、健康检查等高级功能可在后续版本中添加
- **实用性导向**：每个设计决策都以解决实际问题为目标

**核心结构设计：**

```rust
// crates/agent_manager/src/lib.rs

/// Agent 管理器
pub struct AgentManager {
    config: AgentServersConfig,                 // Agent 服务器配置
    env_resolver: EnvironmentVariableResolver,  // 环境变量解析器
    lifecycle_manager: AgentLifecycleManager,   // 生命周期管理
    installation_manager: AgentInstallationManager, // 安装管理器
}

/// Agent 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub agent_id: String,              // Agent 唯一标识
    pub agent_type: AgentType,         // Agent 类型
    pub command: String,               // 启动命令
    pub args: Vec<String>,             // 命令参数
    pub env: HashMap<String, String>,  // 环境变量
    pub installation: InstallationConfig, // 安装配置
    pub enabled: bool,                 // 是否启用
    pub metadata: HashMap<String, String>, // 元数据
}

/// 安装配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationConfig {
    pub package_manager: PackageManager, // 包管理器类型
    pub package_name: Option<String>,    // 包名
    pub version: Option<String>,         // 版本约束
    pub source: Option<String>,          // 安装源
    pub validate_command: Option<Vec<String>>, // 验证命令
    pub auto_update: bool,               // 是否自动更新
}

/// 包管理器类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PackageManager {
    Npm,          // npm 包管理器
    Local,        // 本地二进制
    Custom(String), // 自定义管理器
}

/// Agent 实例
pub struct AgentInstance {
    pub config: AgentConfig,           // 配置信息
    pub process: Option<AgentProcess>, // 进程句柄
    pub status: AgentStatus,          // 运行状态
    pub started_at: Option<DateTime<Utc>>, // 启动时间
}

/// Agent 状态
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Stopped,     // 已停止
    Starting,    // 启动中
    Running,     // 运行中
    Stopping,    // 停止中
    Error(String), // 错误状态
    Unknown,     // 未知状态
}

/// Agent 管理器实现
impl AgentManager {
    /// 创建新的 Agent 管理器
    /// 
    /// # 参数
    /// - `config`: Agent 服务器配置结构体
    /// - `env_resolver`: 环境变量解析器
    pub fn new(config: AgentServersConfig, env_resolver: EnvironmentVariableResolver) -> Result<Self, AgentManagerError>;
    
    /// 启动 Agent
    pub async fn start_agent(&mut self, agent_id: &str, project_id: &str, model_provider: &ModelProviderConfig) -> Result<AgentInstance, AgentManagerError>;
    
    /// 停止 Agent
    pub async fn stop_agent(&mut self, agent_id: &str) -> Result<(), AgentManagerError>;
    
    /// 获取 Agent 状态
    pub fn get_agent_status(&self, agent_id: &str) -> Option<AgentStatus>;
    
    /// 列出所有 Agent
    pub fn list_agents(&self) -> Vec<&AgentConfig>;
    
    /// 列出启用的 Agent
    pub fn list_enabled_agents(&self) -> Vec<&AgentConfig>;
    
    /// 🔥 新增：检查 Agent 是否空闲
    /// 
    /// # 参数
    /// - `project_id`: 项目ID（因为一个项目对应一个Agent）
    /// 
    /// # 返回值
    /// - `Some(true)`: Agent 空闲，可以接收新任务
    /// - `Some(false)`: Agent 正在执行任务
    /// - `None`: Agent 不存在或已停止
    pub fn is_agent_idle(&self, project_id: &str) -> Option<bool> {
        self.lifecycle_manager.is_agent_idle(project_id)
    }
    
    /// 🔥 新增：获取 Agent 详细的空闲状态信息
    pub fn get_agent_idle_status(&self, project_id: &str) -> Option<AgentIdleStatus> {
        self.lifecycle_manager.get_agent_idle_status(project_id)
    }
    
    /// 🔥 新增：列出所有空闲的 Agent
    pub fn list_idle_agents(&self) -> Vec<String> {
        let mut idle_agents = Vec::new();
        
        // 遍历所有启用的 Agent，检查是否空闲
        for agent_config in self.config.get_enabled_agents() {
            if let Some(is_idle) = self.is_agent_idle(&agent_config.agent_id) {
                if is_idle {
                    idle_agents.push(agent_config.agent_id.clone());
                }
            }
        }
        
        idle_agents
    }
    
    /// 🔥 新增：获取 Agent 空闲统计信息
    pub fn get_idle_statistics(&self) -> AgentIdleStatistics {
        let enabled_agents = self.config.get_enabled_agents();
        let mut idle_count = 0;
        let mut active_count = 0;
        let mut unknown_count = 0;
        
        for agent_config in enabled_agents {
            match self.is_agent_idle(&agent_config.agent_id) {
                Some(true) => idle_count += 1,
                Some(false) => active_count += 1,
                None => unknown_count += 1,
            }
        }
        
        AgentIdleStatistics {
            total_enabled: enabled_agents.len(),
            idle_count,
            active_count,
            unknown_count,
        }
    }
    
    /// 验证 Agent 配置
    pub async fn validate_agent_config(&self, agent_config: &AgentConfig) -> Result<ValidationResult, AgentManagerError>;
    
    /// 安装 Agent
    pub async fn install_agent(&self, agent_config: &AgentConfig) -> Result<(), AgentManagerError>;
    
    /// 更新 Agent
    pub async fn update_agent(&self, agent_id: &str) -> Result<(), AgentManagerError>;
}

/// 🔥 新增：Agent 空闲统计信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdleStatistics {
    /// 启用的 Agent 总数
    pub total_enabled: usize,
    /// 空闲的 Agent 数量
    pub idle_count: usize,
    /// 正在执行任务的 Agent 数量
    pub active_count: usize,
    /// 状态未知的 Agent 数量
    pub unknown_count: usize,
}

```

**配置解析和验证工具：**

```rust
impl AgentServersConfig {
    /// 从 JSON 文件加载配置
    pub async fn from_file(path: &Path) -> Result<Self, ConfigError>;
    
    /// 从 JSON 字符串解析配置
    pub fn from_json(json: &str) -> Result<Self, ConfigError>;
    
    /// 验证配置完整性
    pub fn validate(&self) -> Result<(), ConfigError>;
    
    /// 获取启用的 Agent 配置
    pub fn get_enabled_agents(&self) -> Vec<&AgentConfig>;
    
    /// 根据 ID 获取 Agent 配置
    pub fn get_agent(&self, agent_id: &str) -> Option<&AgentConfig>;
}

/// 环境变量解析器
pub struct EnvironmentVariableResolver {
    mappings: HashMap<String, String>,
    custom_resolvers: HashMap<String, fn(&ResolutionContext) -> String>,
}

impl EnvironmentVariableResolver {
    /// 创建包含标准映射的解析器
    pub fn with_standard_mappings() -> Self {
        let mut mappings = HashMap::new();
        
        // ModelProvider 相关 - 完整映射所有字段
        mappings.insert("MODEL_PROVIDER_ID".to_string(), "MODEL_PROVIDER_ID");
        mappings.insert("MODEL_PROVIDER_NAME".to_string(), "MODEL_PROVIDER_NAME");
        mappings.insert("MODEL_PROVIDER_API_KEY".to_string(), "MODEL_PROVIDER_API_KEY");
        mappings.insert("MODEL_PROVIDER_DEFAULT_MODEL".to_string(), "MODEL_PROVIDER_DEFAULT_MODEL");
        mappings.insert("MODEL_PROVIDER_BASE_URL".to_string(), "MODEL_PROVIDER_BASE_URL");
        mappings.insert("MODEL_PROVIDER_REQUIRES_OPENAI_AUTH".to_string(), "MODEL_PROVIDER_REQUIRES_OPENAI_AUTH");
        mappings.insert("MODEL_PROVIDER_API_PROTOCOL".to_string(), "MODEL_PROVIDER_API_PROTOCOL");
        
        // 项目相关
        mappings.insert("PROJECT_ID".to_string(), "PROJECT_ID");
        mappings.insert("PROJECT_NAME".to_string(), "PROJECT_NAME");
        mappings.insert("PROJECT_PATH".to_string(), "PROJECT_PATH");
        
        // MCP 服务器相关
        mappings.insert("CONTEXT7_API_KEY".to_string(), "CONTEXT7_API_KEY");
        mappings.insert("FETCH_TIMEOUT".to_string(), "FETCH_TIMEOUT");
        
        Self {
            mappings,
            custom_resolvers: HashMap::new(),
        }
    }
    
    /// 解析 Agent 配置中的环境变量
    pub fn resolve_agent_config(&self, agent_config: &mut AgentConfig, model_provider: &ModelProviderConfig, project_context: &ProjectContext) -> Result<(), ConfigError> {
        let context = ResolutionContext {
            model_provider: model_provider.clone(),
            project_context: project_context.clone(),
            custom_variables: HashMap::new(),
            mcp_variables: HashMap::new(),
        };
        
        // 解析环境变量值
        for (key, value) in agent_config.env.iter_mut() {
            *value = self.resolve_value(value, &context);
        }
        
        // 解析命令参数中的变量
        for arg in agent_config.args.iter_mut() {
            *arg = self.resolve_value(arg, &context);
        }
        
        Ok(())
    }
    
    /// 解析单个环境变量值
    pub fn resolve_value(&self, template: &str, context: &ResolutionContext) -> String {
        let mut result = template.to_string();
        
        // 替换 ModelProvider 相关变量
        result = result.replace("{MODEL_PROVIDER_ID}", &context.model_provider.id);
        result = result.replace("{MODEL_PROVIDER_NAME}", &context.model_provider.name);
        result = result.replace("{MODEL_PROVIDER_API_KEY}", &context.model_provider.api_key);
        result = result.replace("{MODEL_PROVIDER_DEFAULT_MODEL}", &context.model_provider.default_model);
        result = result.replace("{MODEL_PROVIDER_BASE_URL}", &context.model_provider.base_url);
        result = result.replace("{MODEL_PROVIDER_REQUIRES_OPENAI_AUTH}", 
                               &context.model_provider.requires_openai_auth.to_string());
        result = result.replace("{MODEL_PROVIDER_API_PROTOCOL}", 
                               context.model_provider.api_protocol.as_ref().unwrap_or(&String::new()));
        
        // 替换项目相关变量
        result = result.replace("{PROJECT_ID}", &context.project_context.project_id);
        result = result.replace("{PROJECT_NAME}", &context.project_context.project_name);
        result = result.replace("{PROJECT_PATH}", &context.project_context.project_path.display().to_string());
        
        // 替换 MCP 变量
        for (key, value) in &context.mcp_variables {
            result = result.replace(&format!("{{{}}}", key), value);
        }
        
        // 替换自定义变量
        for (key, value) in &context.custom_variables {
            result = result.replace(&format!("{{{}}}", key), value);
        }
        
        result
    }
    
    /// 🔥 新增：解析系统提示词模板
    /// 根据配置的系统提示词模板解析最终内容
    pub fn resolve_system_prompt(&self, system_prompt_config: &Option<SystemPromptConfig>, context: &ResolutionContext) -> Option<String> {
        match system_prompt_config {
            Some(config) if config.enabled => {
                let resolved = self.resolve_value(&config.template, context);
                Some(resolved)
            }
            Some(_) => None, // 明确禁用时返回 None
            None => None,     // 未配置时返回 None
        }
    }
    
    /// 🔥 新增：解析用户提示词包装
    /// 根据配置的用户提示词模板包装用户的实际输入
    pub fn resolve_user_prompt(&self, user_input: &str, user_prompt_config: &Option<UserPromptConfig>) -> String {
        match user_prompt_config {
            Some(config) if config.enabled => {
                config.template.replace("{user_prompt}", user_input)
            }
            _ => user_input.to_string(), // 如果未启用或未配置，直接返回原输入
        }
    }
    
    /// 添加自定义变量映射
    pub fn add_mapping(&mut self, key: String, value: String) {
        self.mappings.insert(key, value);
    }
}

/// 解析上下文
pub struct ResolutionContext {
    pub model_provider: ModelProviderConfig,
    pub project_context: ProjectContext,
    pub custom_variables: HashMap<String, String>,
    pub mcp_variables: HashMap<String, String>,
}
```

**安装管理设计（容器环境优化）：**

```rust
/// Agent 安装管理器
/// 容器环境下的简化安装管理，只负责安装验证和更新
pub struct AgentInstallationManager {
    installers: HashMap<PackageManager, Box<dyn AgentInstaller>>,
    install_dir: PathBuf,
}

impl AgentInstallationManager {
    /// 注册安装器
    pub fn register_installer(&mut self, package_manager: PackageManager, installer: Box<dyn AgentInstaller>);
    
    /// 安装 Agent
    pub async fn install_agent(&self, config: &InstallationConfig) -> Result<InstallResult, InstallationError>;
    
    /// 验证安装
    pub async fn validate_installation(&self, config: &InstallationConfig) -> Result<bool, InstallationError>;
    
    /// 更新 Agent
    pub async fn update_agent(&self, config: &InstallationConfig) -> Result<InstallResult, InstallationError>;
}

/// Agent 安装器接口
#[async_trait]
pub trait AgentInstaller: Send + Sync {
    /// 安装 Agent
    async fn install(&self, config: &InstallationConfig, install_dir: &Path) -> Result<InstallResult, InstallationError>;
    
    /// 验证安装
    async fn validate(&self, config: &InstallationConfig, install_dir: &Path) -> Result<bool, InstallationError>;
    
    /// 更新 Agent
    async fn update(&self, config: &InstallationConfig, install_dir: &Path) -> Result<InstallResult, InstallationError>;
    
    /// 获取安装器类型
    fn package_manager(&self) -> PackageManager;
}
```

**生命周期管理设计（核心功能）：**

```rust
/// Agent 生命周期管理器
pub struct AgentLifecycleManager {
    processes: DashMap<String, AgentProcess>,
    // 引用现有的 PROJECT_AND_AGENT_INFO_MAP 进行状态管理
    agent_status_map: DashMap<String, AgentStatusInfo>,
}

/// Agent 状态信息（基于现有实现的抽象）
#[derive(Debug, Clone)]
pub struct AgentStatusInfo {
    pub status: AgentStatus,           // Active/Idle/Terminating
    pub session_id: Option<String>,   // 当前会话ID
    pub request_id: Option<String>,   // 当前请求ID
    pub last_activity: DateTime<Utc>, // 最后活动时间
    pub created_at: DateTime<Utc>,    // 创建时间
}

impl AgentLifecycleManager {
    /// 启动 Agent 进程
    pub async fn start_agent(&self, config: &AgentConfig, context: &AgentContext) -> Result<AgentProcess, LifecycleError>;
    
    /// 停止 Agent 进程
    pub async fn stop_agent(&self, agent_id: &str) -> Result<(), LifecycleError>;
    
    /// 重启 Agent
    pub async fn restart_agent(&self, agent_id: &str) -> Result<AgentProcess, LifecycleError>;
    
    /// 获取进程状态
    pub fn get_process_status(&self, agent_id: &str) -> Option<ProcessStatus>;
    
    /// 🔥 新增：获取 Agent 是否空闲状态
    pub fn is_agent_idle(&self, agent_id: &str) -> Option<bool> {
        self.agent_status_map.get(agent_id)
            .map(|info| matches!(info.status, AgentStatus::Idle))
    }
    
    /// 🔥 新增：获取 Agent 详细的空闲状态信息
    pub fn get_agent_idle_status(&self, agent_id: &str) -> Option<AgentIdleStatus> {
        self.agent_status_map.get(agent_id).map(|info| {
            AgentIdleStatus {
                is_idle: matches!(info.status, AgentStatus::Idle),
                current_status: info.status.clone(),
                last_activity: info.last_activity,
                session_id: info.session_id.clone(),
                current_request_id: info.request_id.clone(),
                idle_duration: Utc::now().signed_duration_since(info.last_activity).to_std().unwrap_or_default(),
            }
        })
    }
    
    /// 🔥 新增：更新 Agent 状态为 Active（开始执行任务）
    pub fn set_agent_active(&self, agent_id: &str, request_id: Option<String>) {
        if let Some(mut info) = self.agent_status_map.get_mut(agent_id) {
            info.status = AgentStatus::Active;
            info.last_activity = Utc::now();
            info.request_id = request_id;
        }
    }
    
    /// 🔥 新增：更新 Agent 状态为 Idle（任务完成）
    pub fn set_agent_idle(&self, agent_id: &str) {
        if let Some(mut info) = self.agent_status_map.get_mut(agent_id) {
            info.status = AgentStatus::Idle;
            info.last_activity = Utc::now();
            info.request_id = None;
        }
    }
}

/// 🔥 新增：Agent 空闲状态响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdleStatus {
    /// 是否空闲
    pub is_idle: bool,
    /// 当前状态
    pub current_status: AgentStatus,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 当前会话ID
    pub session_id: Option<String>,
    /// 当前请求ID（如果正在执行任务）
    pub current_request_id: Option<String>,
    /// 空闲持续时间
    pub idle_duration: Duration,
}

/// Agent 进程封装
pub struct AgentProcess {
    pub id: String,
    pub child: tokio::process::Child,
    pub config: AgentConfig,
    pub start_time: DateTime<Utc>,
}
```

**状态管理机制分析：**

**现有实现发现：**
1. **状态存储**：使用 `PROJECT_AND_AGENT_INFO_MAP` 全局存储 Agent 状态
2. **状态类型**：`AgentStatus` 枚举包含 `Active`、`Idle`、`Terminating` 三种状态
3. **状态更新**：在 `channel_utils.rs` 中通过 ACP 消息触发状态切换
4. **状态切换时机**：
   - `Active`: 收到 Prompt 请求时
   - `Idle`: Prompt 处理完成或被取消时
   - `Terminating`: Agent 停止过程中

**设计优化：**
- **复用现有机制**：Agent 管理器封装现有状态管理逻辑
- **提供统一接口**：标准化的状态查询方法
- **性能优化**：避免重复的状态存储和管理
- **扩展性**：为未来状态扩展预留接口

**🔥 新增：ACP 连接池管理的完整使用示例：**

```rust
// 1. 加载配置（由调用方负责）
let agent_config = AgentServersConfig::from_file("/etc/rcoder/agents.json").await?;
let env_resolver = EnvironmentVariableResolver::with_standard_mappings();

// 2. 🔥 新增：初始化 ACP 连接池管理器
let acp_config = AcpConnectionConfig::default();
let acp_connection_manager = Arc::new(AcpConnectionManager::new(acp_config));

// 3. 准备 ModelProvider 配置
let model_provider = ModelProviderConfig {
    id: "anthropic-claude".to_string(),
    name: "Claude".to_string(),
    base_url: "https://api.anthropic.com".to_string(),
    api_key: "sk-ant-xxx".to_string(),
    requires_openai_auth: false,
    default_model: "claude-3-5-sonnet-20241022".to_string(),
    api_protocol: Some("anthropic".to_string()),
};

// 4. 准备项目上下文
let project_context = ProjectContext {
    project_id: "project-123".to_string(),
    project_name: "my-react-app".to_string(),
    project_path: PathBuf::from("/workspace/project-123"),
};

// 5. 🔥 新增：创建 Agent 工厂，集成 ACP 连接池
let agent_factory = Arc::new(AgentFactory::new(
    registry,
    launcher,
    config_manager,
    mcp_manager,
    acp_connection_manager.clone(),
));

// 6. 🔥 新增：通过连接池获取 Agent 连接
let agent_connection = acp_connection_manager.get_or_create_connection(
    "claude-code-acp",
    &agent_config.get_agent("claude-code-acp").unwrap(),
    &model_provider,
    &project_context,
).await?;

// 7. 🔥 新增：使用连接池发送提示词
let prompt_request = PromptRequest {
    prompt: "帮我实现一个 React 登录组件".to_string(),
    session_id: Some("session-123".to_string()),
    model_provider: Some(model_provider.clone()),
};

let response = acp_connection_manager.send_prompt("claude-code-acp", prompt_request).await?;

// 8. 🔥 新增：处理提示词包装
let user_input = "帮我实现一个登录组件";
let agent_spec = agent_config.get_agent("claude-code-acp").unwrap();
let wrapped_prompt = env_resolver.resolve_user_prompt(
    user_input,
    &agent_spec.user_prompt,
);

// 9. 🔥 新增：处理系统提示词
let system_prompt = env_resolver.resolve_system_prompt(
    &agent_spec.system_prompt,
    &ResolutionContext {
        model_provider: model_provider.clone(),
        project_context: project_context.clone(),
        custom_variables: HashMap::new(),
        mcp_variables: HashMap::new(),
    },
);

// 10. 🔥 新增：获取连接池统计信息
let stats = acp_connection_manager.get_connection_stats();
println!("当前连接数: {}/{}", stats.total_connections, stats.max_connections);
println!("最大空闲时间: {:?}", stats.max_idle_time);

// 11. 🔥 新增：取消长时间运行的任务
let cancel_notification = CancelNotification {
    session_id: "session-123".to_string(),
    reason: "用户取消请求".to_string(),
};

acp_connection_manager.cancel_request("claude-code-acp", cancel_notification).await?;
```

**传统 Agent 管理器使用示例：**

```rust
// 对于不需要 ACP 连接池的场景，仍可使用传统方式

// 1. 初始化 Agent 管理器
let mut agent_manager = AgentManager::new(agent_config, env_resolver)?;

// 2. 启动 Agent（根据配置中的 agent_type 自动选择启动方式）
let agent_instance = agent_manager.start_agent(
    "claude-code-acp", 
    "project-123", 
    &model_provider
).await?;

// 6. Agent 启动后，环境变量会被自动解析：
//    - ANTHROPIC_API_KEY = "sk-ant-xxx" (来自 MODEL_PROVIDER_API_KEY)
//    - ANTHROPIC_BASE_URL = "https://api.anthropic.com" (来自 MODEL_PROVIDER_BASE_URL)
//    - ANTHROPIC_MODEL = "claude-3-5-sonnet-20241022" (来自 MODEL_PROVIDER_DEFAULT_MODEL)
```

**环境变量解析示例：**

假设 Agent 配置中有以下环境变量：

```json
{
  "env": {
    "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
    "ANTHROPIC_BASE_URL": "{MODEL_PROVIDER_BASE_URL}",
    "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}",
    "PROJECT_DIR": "{PROJECT_PATH}",
    "AGENT_ID": "{PROJECT_ID}-claude",
    "CONTEXT7_API_KEY": "{CONTEXT7_API_KEY}"
  }
}
```

经过 `EnvironmentVariableResolver::resolve_agent_config()` 处理后：

```json
{
  "env": {
    "ANTHROPIC_API_KEY": "sk-ant-xxx",
    "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
    "ANTHROPIC_MODEL": "claude-3-5-sonnet-20241022",
    "PROJECT_DIR": "/workspace/project-123",
    "AGENT_ID": "project-123-claude",
    "CONTEXT7_API_KEY": "ctx7-xxx"  // 来自 MCP 变量配置
  }
}
```

**🔥 新增：用户提示词包装使用示例：**

```rust
// 1. 用户输入包装示例
let user_input = "帮我写一个 React 组件";
let agent_config = agent_manager.get_agent("claude-code-acp").unwrap();

// 2. 应用用户提示词包装
let wrapped_prompt = env_resolver.resolve_user_prompt(
    user_input, 
    &agent_config.user_prompt
);

// 3. 包装后的结果：
// "你好 帮我写一个 React 组件，请帮我分析这个问题并提供详细的解决方案。"

// 4. 将包装后的提示词发送给 Agent
let response = agent.send_prompt(&wrapped_prompt).await?;
```

**配置示例和使用效果：**

```json
{
  "agent_servers": {
    "react-developer": {
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}"
      },
      "system_prompt": {
        "template": "你是一个专业的 React 开发助手"
      },
      "user_prompt": {
        "template": "作为 React 专家，请帮我解决以下问题：{user_prompt}。请提供现代、可维护的代码示例。"
      }
    },
    "rust-developer": {
      "command": "claude-code-acp", 
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}"
      },
      "system_prompt": {
        "template": "你是一个 Rust 系统编程专家"
      },
      "user_prompt": {
        "enabled": false,
        "template": "{user_prompt}"
      }
    },
    "general-assistant": {
      "command": "kimi",
      "args": ["--acp"],
      "env": {
        "KIMI_API_KEY": "{MODEL_PROVIDER_API_KEY}"
      },
      "system_prompt": {
        "enabled": false,
        "template": "你是一个通用 AI 助手"
      },
      "user_prompt": null  // 未配置，直接使用原输入
    }
  }
}
```

**实际使用效果演示：**

```rust
// 示例 1：React Developer Agent
let user_input = "如何实现一个自定义 Hook？";
let wrapped = env_resolver.resolve_user_prompt(
    user_input, 
    &react_config.user_prompt
);
// 结果："作为 React 专家，请帮我解决以下问题：如何实现一个自定义 Hook？。请提供现代、可维护的代码示例。"

// 示例 2：Rust Developer Agent (禁用包装)
let user_input = "解释 Rust 的所有权机制";
let wrapped = env_resolver.resolve_user_prompt(
    user_input, 
    &rust_config.user_prompt  
);
// 结果："解释 Rust 的所有权机制" (直接返回原输入)

// 示例 3：General Assistant Agent (未配置)
let user_input = "今天天气怎么样？";
let wrapped = env_resolver.resolve_user_prompt(
    user_input, 
    &general_config.user_prompt
);
// 结果："今天天气怎么样？" (直接返回原输入)
```

**user_prompt 配置特点：**

1. **灵活的模板系统**：支持在模板中使用 `{user_prompt}` 占位符
2. **条件启用**：通过 `enabled` 字段控制是否启用包装功能
3. **简化配置**：未配置或禁用时直接使用用户原输入
4. **Agent 个性化**：每个 Agent 可以有自己独特的用户提示词包装风格
5. **动态替换**：运行时根据用户输入动态生成最终提示词

**🔥 新增：系统提示词配置特点：**

1. **统一的配置格式**：`system_prompt` 和 `user_prompt` 都使用相同的 `{ "template": "...", "enabled": true/false }` 格式
2. **默认启用**：`enabled` 字段默认为 `true`，可以省略不写
3. **模板变量支持**：系统提示词也支持所有环境变量替换（如 `{MODEL_PROVIDER_DEFAULT_MODEL}`）
4. **灵活控制**：可以通过 `enabled: false` 禁用特定的系统提示词

**system_prompt vs user_prompt 对比：**

| 特性 | system_prompt | user_prompt |
|------|---------------|-------------|
| **用途** | 设置 Agent 的角色和行为 | 包装用户的每次输入 |
| **变量支持** | 支持所有环境变量 | 只支持 `{user_prompt}` 占位符 |
| **默认状态** | `enabled: true` | `enabled: true` |
| **配置示例** | `{ "template": "你是React专家" }` | `{ "template": "请帮我：{user_prompt}" }` |

**新的完整配置示例：**

```json
{
  "agent_servers": {
    "full-stack-developer": {
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
        "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}"
      },
      "system_prompt": {
        "template": "你是 {MODEL_PROVIDER_NAME} 驱动的全栈开发专家，专注于 {PROJECT_NAME} 项目。使用现代开发实践，代码要简洁、可维护、性能优化。"
      },
      "user_prompt": {
        "template": "你是RCoder，一个专业的AI编程助手。\n\n## 核心身份与职责\n- 专业的编程助手，帮助用户解决编程问题\n- 提供简洁、实用、可执行的代码解决方案\n- 遵循最佳实践，编写高质量代码\n- 始终将用户需求放在首位\n\n## 代码格式规范\n- 优先使用现代语言特性和标准库\n- 变量和函数命名使用清晰、描述性的英文名称\n- 保持代码简洁，避免过度复杂的抽象\n- 使用适当的注释解释关键逻辑\n\n## 开发约束\n- 避免添加未请求的功能，保持解决方案专注\n- 优先选择最简单有效的实现方式\n- 不要为未来可能的需求添加复杂性\n- 确保代码安全、可维护\n\n## MCP工具使用指导\n- 合理使用可用的工具来辅助开发任务\n- 当需要文件操作、搜索、测试时使用相应的工具\n- 根据上下文选择最合适的工具\n\n## 思考要求\n- 在回答前进行充分的思考和分析\n- 确保解决方案的完整性和正确性\n- 提供清晰、有条理的回答\n\n用户请求：\n{user_prompt}"
      },
      "installation": {
        "package_manager": "npm",
        "package_name": "@zed-industries/claude-code-acp"
      }
    },
    "code-reviewer": {
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}"
      },
      "system_prompt": {
        "enabled": true,
        "template": "你是一个资深代码审查专家，专注于代码质量、安全性和最佳实践。提供具体的改进建议。"
      },
      "user_prompt": {
        "enabled": true,
        "template": "请审查以下代码，重点关注可读性、性能和安全性：\n\n{user_prompt}\n\n请提供具体的改进建议。"
      }
    },
    "minimal-agent": {
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}"
      },
      "system_prompt": {
        "enabled": false
      },
      "user_prompt": null  // 不包装用户输入
    }
  }
}
```

**运行时解析示例：**

```rust
// 1. 解析系统提示词
let system_prompt = env_resolver.resolve_system_prompt(
    &agent_config.system_prompt, 
    &context
);
// 结果：如果 enabled=true 且模板包含变量，会返回解析后的内容
// 结果：如果 enabled=false 或未配置，会返回 None

// 2. 解析用户提示词包装
let user_input = "帮我实现一个登录组件";
let wrapped_prompt = env_resolver.resolve_user_prompt(
    user_input, 
    &agent_config.user_prompt
);
// 结果："你好，我是 my-project 项目的开发者。帮我实现一个登录组件 请提供详细的解决方案和代码示例。"
```

**适用场景：**

- **专业领域 Agent**：为特定技术领域的 Agent 添加领域特定的引导语
- **教学辅导 Agent**：在用户问题前添加启发式引导
- **代码审查 Agent**：标准化代码审查请求的格式
- **多语言支持**：为不同语言的 Agent 添加相应的语言引导
- **项目定制化**：系统提示词中注入项目名称、环境变量等信息
- **角色切换**：通过 `enabled` 字段动态控制 Agent 的角色

println!("Agent started: {:?}", agent_instance.status);

// 🔥 新增：获取 Agent 当前配置
if let Some(config) = agent.get_config(&agent_instance) {
    println!("Agent current config:");
    println!("  - Agent type: {:?}", config.agent_type);
    println!("  - Command: {}", config.command);
    println!("  - Args: {:?}", config.args);
}

// 🔥 新增：重启 Agent（比如配置更新后）
let new_config = AgentConfig { /* 新配置 */ };
let new_context = AgentContext { /* 新上下文 */ };
let restarted_instance = agent.restart(&agent_instance, new_config, new_context).await?;
println!("Agent restarted: {:?}", restarted_instance.status);

// 获取所有启用的 Agent
let enabled_agents = agent_manager.list_enabled_agents();
for agent in enabled_agents {
    println!("Enabled agent: {} ({})", agent.agent_id, agent.command);
}

// 🔥 新增：检查 Agent 空闲状态
let project_id = "project-123";
match agent_manager.is_agent_idle(project_id) {
    Some(true) => println!("Agent for project {} is idle and ready for new tasks", project_id),
    Some(false) => println!("Agent for project {} is currently busy", project_id),
    None => println!("Agent for project {} is not running", project_id),
}

// 🔥 新增：获取详细的空闲状态信息
if let Some(idle_status) = agent_manager.get_agent_idle_status(project_id) {
    println!("Agent status details:");
    println!("  - Is idle: {}", idle_status.is_idle);
    println!("  - Current status: {:?}", idle_status.current_status);
    println!("  - Last activity: {}", idle_status.last_activity);
    println!("  - Session ID: {:?}", idle_status.session_id);
    println!("  - Current request ID: {:?}", idle_status.current_request_id);
    println!("  - Idle duration: {:?}", idle_status.idle_duration);
}

// 🔥 新增：列出所有空闲的 Agent
let idle_agents = agent_manager.list_idle_agents();
println!("Idle agents: {:?}", idle_agents);

// 🔥 新增：获取空闲统计信息
let stats = agent_manager.get_idle_statistics();
println!("Agent statistics:");
println!("  - Total enabled: {}", stats.total_enabled);
println!("  - Idle: {}", stats.idle_count);
println!("  - Active: {}", stats.active_count);
println!("  - Unknown: {}", stats.unknown_count);
```

**启动流程说明：**
Agent 管理器根据配置中的 `agent_type` 字段自动选择启动方式：
- **"claude"**: 使用 Claude Code ACP 启动流程
- **"kimi"**: 使用 Kimi CLI 启动流程  
- **"custom"**: 使用自定义 Agent 启动流程
- **其他类型**: 通用启动流程

这种方式避免了动态注册的复杂性，让 Agent 类型通过配置文件驱动。

**配置文件结构（JSON 格式）：**

```json
{
  "agent_servers": {
    "claude-code-acp": {
    "agent_type": "claude",
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_API_KEY": "{MODEL_PROVIDER_API_KEY}",
        "ANTHROPIC_BASE_URL": "{MODEL_PROVIDER_BASE_URL}",
        "ANTHROPIC_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}",
        "RUST_LOG": "info"
      },
      "installation": {
        "package_manager": "npm",
        "package_name": "@zed-industries/claude-code-acp",
        "version": "latest",
        "validate_command": ["claude-code-acp", "--version"],
        "auto_update": true
      },
      "enabled": true,
      "metadata": {
        "description": "Claude Code ACP Agent",
        "version": "1.0.0",
        "maintainer": "Anthropic"
      }
    }

  "kimi-cli": {
      "agent_type": "kimi",
      "command": "kimi",
      "args": ["--acp"],
      "env": {
        "KIMI_API_KEY": "{MODEL_PROVIDER_API_KEY}",
        "KIMI_MODEL": "{MODEL_PROVIDER_DEFAULT_MODEL}",
        "KIMI_BASE_URL": "{MODEL_PROVIDER_BASE_URL}"
      },
      "installation": {
        "package_manager": "npm",
        "package_name": "@kimi-ai/cli",
        "version": "^1.0.0",
        "auto_update": false
      },
      "enabled": true
    }
  },
  "global": {
    "install_dir": "/opt/rcoder/agents",
    "log_dir": "/var/log/rcoder/agents",
    "default_timeout": 30,
    "max_concurrent_agents": 10
  }
```

#### 4.4.5 ACP 连接池管理

**🔥 重要：避免死锁的 ACP 连接池设计**

基于当前 RCoder 工程的深入分析，发现以下死锁风险点：
1. **DashMap 嵌套访问**：多个 DashMap 同时访问可能造成死锁
2. **RAII 与显式清理冲突**：生命周期管理和手动状态更新协调不当
3. **状态更新时序问题**：Agent 状态和连接状态同步更新的竞争条件

**🔧 死锁预防设计原则：**

1. **单一数据源原则**：每个 Agent 只在一个地方管理状态
2. **无嵌套锁**：避免在持有锁时访问其他可能被锁的资源
3. **RAII 优先**：以 RAII 为主要资源管理方式，避免显式清理
4. **原子操作**：状态更新使用原子性操作，避免中间状态
5. **单向依赖**：建立清晰的依赖层次，避免循环依赖

**重新设计的 ACP 连接池管理器：**

```rust
/// ACP 连接池管理器
/// 
/// 🔥 无死锁风险设计：每个 Agent 使用独立的连接实例，避免共享状态
pub struct AcpConnectionManager {
    /// 连接池：agent_id -> AgentConnection
    /// ✅ 基于当前工程成功的DashMap模式，避免传统锁的竞争问题
    connections: Arc<DashMap<String, Weak<AgentConnection>>>,
    
    /// 连接配置
    config: Arc<AcpConnectionConfig>,
    
    /// 后台清理任务句柄
    cleanup_task: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
}

/// ACP 连接配置
#[derive(Debug, Clone)]
pub struct AcpConnectionConfig {
    /// 最大空闲时间，超过此时间的连接将被清理
    pub max_idle_time: Duration,
    
    /// 清理任务间隔
    pub cleanup_interval: Duration,
    
    /// 连接超时时间
    pub connection_timeout: Duration,
    
    /// 最大连接数
    pub max_connections: usize,
}

impl Default for AcpConnectionConfig {
    fn default() -> Self {
        Self {
            max_idle_time: Duration::from_secs(300),      // 5分钟
            cleanup_interval: Duration::from_secs(60),    // 1分钟清理一次
            connection_timeout: Duration::from_secs(30),   // 30秒连接超时
            max_connections: 100,                           // 最大100个连接
        }
    }
}

/// Agent 连接包装器
/// 
/// 🔥 与 RAII 模式兼容的设计：AgentConnection 本身就是 RAII 资源
pub struct AgentConnection {
    /// Agent 唯一标识
    pub agent_id: String,
    
    /// LocalSet 实例（必须在这个 LocalSet 中使用 ACP 连接）
    /// 📌 使用 Box<LocalSet> 避免与全局 LocalSet 冲突
    local_set: Box<LocalSet>,
    
    /// 客户端连接（非 Send，只能在 LocalSet 内使用）
    /// 📌 使用 RefCell 而不是 Mutex，避免与全局状态锁冲突
    client_conn: RefCell<Option<ClientSideConnection>>,
    
    /// 生命周期管理器（与现有的 AgentLifecycleGuard 集成）
    lifecycle_guard: AgentLifecycleGuard,
    
    /// 最后活动时间（使用原子类型，避免锁）
    last_activity: AtomicInstant,
    
    /// 连接创建时间
    created_at: Instant,
    
    /// 连接状态（使用原子操作，避免锁竞争）
    status: AtomicU8, // 存储 ConnectionStatus 的数字表示
    
    /// 连接管理器的弱引用，用于自动清理
    manager_weak: Weak<AcpConnectionManager>,
}

/// 🔥 原子时间戳包装器，避免使用 Mutex
#[derive(Debug)]
pub struct AtomicInstant {
    inner: AtomicU64,
}

impl AtomicInstant {
    pub fn new() -> Self {
        Self {
            inner: AtomicU64::new(0),
        }
    }
    
    pub fn store(&self, instant: Instant) {
        self.inner.store(instant.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
    
    pub fn load(&self) -> Instant {
        let nanos = self.inner.load(Ordering::Relaxed);
        if nanos == 0 {
            Instant::now()
        } else {
            Instant::now() - Duration::from_nanos(nanos)
        }
    }
}

/// 🔥 连接状态（使用数字表示，支持原子操作）
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum ConnectionStatus {
    /// 连接中
    Connecting = 1,
    /// 已连接
    Connected = 2,
    /// 空闲
    Idle = 3,
    /// 错误
    Error = 4,
    /// 已关闭
    Closed = 5,
}

impl ConnectionStatus {
    fn to_u8(self) -> u8 {
        self as u8
    }
    
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Connecting,
            2 => Self::Connected,
            3 => Self::Idle,
            4 => Self::Error,
            5 => Self::Closed,
            _ => Self::Closed,
        }
    }
}

impl AcpConnectionManager {
    /// 创建新的连接池管理器
    /// 
    /// 🎯 基于当前工程成功的DashMap模式，避免传统锁的竞争问题
    pub fn new(config: AcpConnectionConfig) -> Self {
        let manager = Self {
            connections: Arc::new(DashMap::new()), // ✅ 使用DashMap替代RwLock<HashMap>
            config: Arc::new(config),
            cleanup_task: Arc::new(tokio::sync::Mutex::new(None)),
        };
        
        // 启动后台清理任务
        manager.start_cleanup_task();
        manager
    }
    
    /// 🔥 获取或创建 Agent 连接（基于DashMap的无死锁设计）
    /// 
    /// 关键设计点：
    /// 1. 使用DashMap的entry API避免嵌套访问
    /// 2. 基于当前工程验证成功的并发模式
    /// 3. 使用 Weak 引用避免循环依赖
    /// 4. RAII 资源自动清理，无需手动管理
    pub async fn get_or_create_connection(
        &self,
        agent_id: &str,
        agent_config: &AgentConfig,
        model_provider: &ModelProviderConfig,
        project_context: &ProjectContext,
    ) -> Result<Arc<AgentConnection>, AcpError> {
        let agent_id = agent_id.to_string();
        
        // 🎯 使用DashMap的entry API，原子性的检查-创建操作
        let connection_entry = self.connections.entry(agent_id.clone());
        
        match connection_entry {
            dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
                // 现有连接存在
                if let Some(connection) = occupied.get().upgrade() {
                    // 检查连接状态（原子操作）
                    let status = connection.get_status();
                    if status == ConnectionStatus::Connected || status == ConnectionStatus::Idle {
                        // 原子更新最后活动时间
                        connection.update_last_activity();
                        return Ok(connection);
                    } else {
                        // 连接状态异常，清理并继续创建新连接
                        occupied.remove();
                    }
                } else {
                    // Weak引用已失效，清理并继续创建新连接
                    occupied.remove();
                }
            }
            dashmap::mapref::entry::Entry::Vacant(_) => {
                // 没有现有连接，继续创建新连接
            }
        }
        
        // 📌 创建新连接
        let connection = self.create_new_connection(&agent_id, agent_config, model_provider, project_context).await?;
        
        // 📌 注册新连接（原子操作）
        self.connections.insert(agent_id, Arc::downgrade(&connection));
        
        Ok(connection)
    }
    
    /// 🔥 创建新的 ACP 连接（无死锁设计）
    async fn create_new_connection(
        &self,
        agent_id: &str,
        agent_config: &AgentConfig,
        model_provider: &ModelProviderConfig,
        project_context: &ProjectContext,
    ) -> Result<Arc<AgentConnection>, AcpError> {
        // 创建 LocalSet 实例（使用 Box 避免与全局 LocalSet 冲突）
        let local_set = Box::new(LocalSet::new());
        
        // 准备 Agent 配置
        let mut resolved_config = agent_config.clone();
        let env_resolver = EnvironmentVariableResolver::with_standard_mappings();
        env_resolver.resolve_agent_config(&mut resolved_config, model_provider, project_context)?;
        
        // 🔥 关键设计：将连接创建移到独立的作用域，避免闭包捕获 self
        let agent_id_clone = agent_id.to_string();
        let connection_future = async move {
            // 创建进程（在 LocalSet 中执行）
            let mut command = tokio::process::Command::new(&resolved_config.command);
            command.args(&resolved_config.args);
            command.envs(&resolved_config.env);
            
            let child = command.spawn()
                .map_err(|e| AcpError::ProcessError(e.to_string()))?;
            
            // 创建生命周期管理器（与现有工程集成）
            let lifecycle_guard = AgentLifecycleGuard::new(child, agent_id_clone)?;
            
            // 🔥 在 LocalSet 中创建 ACP 连接
            // 这里需要根据具体的 ACP 协议实现
            let client_conn = Self::create_acp_connection_internal(&lifecycle_guard).await?;
            
            Ok((client_conn, lifecycle_guard))
        };
        
        // 在 LocalSet 中执行连接创建
        let (client_conn, lifecycle_guard) = local_set.run_until(connection_future).await?;
        
        // 🔥 创建连接包装器（RAII 模式）
        let connection = Arc::new(AgentConnection {
            agent_id: agent_id.to_string(),
            local_set,
            client_conn: RefCell::new(Some(client_conn)),
            lifecycle_guard,
            last_activity: AtomicInstant::new(),
            created_at: Instant::now(),
            status: AtomicU8::new(ConnectionStatus::Connected.to_u8()),
            manager_weak: Arc::downgrade(&self.connections),
        });
        
        // 初始化最后活动时间
        connection.last_activity.store(Instant::now());
        
        Ok(connection)
    }
    
    /// 🔥 内部 ACP 连接创建方法（避免捕获 self）
    async fn create_acp_connection_internal(
        lifecycle_guard: &AgentLifecycleGuard,
    ) -> Result<ClientSideConnection, AcpError> {
        // 在这里实现具体的 ACP 连接创建逻辑
        // 根据 ACP 协议规范，建立与 Agent 的连接
        
        // 示例实现（需要根据实际 ACP 协议调整）：
        let (stdin, stdout) = lifecycle_guard.get_process_stdio()?;
        let (outgoing, incoming) = create_acp_channels(stdin, stdout)?;
        
        let (client_conn, handle_io) = ClientSideConnection::new(
            client, outgoing, incoming, |fut| {
                tokio::task::spawn_local(fut);
            }
        );
        
        tokio::task::spawn_local(handle_io);
        
        Ok(client_conn)
    }
    
    /// 🔥 发送提示词到 Agent（简化的无死锁接口）
    pub async fn send_prompt(
        &self,
        agent_id: &str,
        prompt_request: PromptRequest,
    ) -> Result<PromptResponse, AcpError> {
        // 获取连接（会自动复用或创建新连接）
        let connection = self.get_or_create_connection(
            agent_id,
            &self.get_default_agent_config()?,
            &self.get_default_model_provider(),
            &ProjectContext::default(),
        ).await?;
        
        // 使用连接自己的方法执行操作
        connection.execute_operation(|client_conn| {
            Box::pin(async move {
                // 在这里实现具体的提示词发送逻辑
                let response = client_conn.send_prompt(prompt_request).await?;
                Ok(response)
            })
        }).await
    }
    
    /// 🔥 取消正在执行的任务（简化的无死锁接口）
    pub async fn cancel_request(
        &self,
        agent_id: &str,
        cancel_notification: CancelNotification,
    ) -> Result<(), AcpError> {
        // 获取连接
        let connection = self.get_or_create_connection(
            agent_id,
            &self.get_default_agent_config()?,
            &self.get_default_model_provider(),
            &ProjectContext::default(),
        ).await?;
        
        // 使用连接自己的方法执行操作
        connection.execute_operation(|client_conn| {
            Box::pin(async move {
                client_conn.send_cancel(cancel_notification).await?;
                Ok(())
            })
        }).await
    }
    
    /// 🔥 获取连接统计信息（无锁设计）
    pub fn get_connection_stats(&self) -> ConnectionStats {
        let connections = self.connections.read().unwrap();
        ConnectionStats {
            total_connections: connections.len(),
            max_connections: self.config.max_connections,
            cleanup_interval: self.config.cleanup_interval,
            max_idle_time: self.config.max_idle_time,
        }
    }
    
    /// 🔥 获取默认 Agent 配置（辅助方法）
    fn get_default_agent_config(&self) -> Result<AgentConfig, AcpError> {
        // 这里应该从配置管理器获取，暂时返回默认配置
        Ok(AgentConfig::default())
    }
    
    /// 🔥 获取默认 ModelProvider（辅助方法）
    fn get_default_model_provider(&self) -> ModelProviderConfig {
        // 这里应该从配置获取，暂时返回默认配置
        ModelProviderConfig::default()
    }
    
    /// 🔥 启动后台清理任务（无死锁设计）
    fn start_cleanup_task(&mut self) {
        let connections = self.connections.clone();
        let max_idle_time = self.config.max_idle_time;
        let cleanup_interval = self.config.cleanup_interval;
        
        let cleanup_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(cleanup_interval);
            
            loop {
                interval.tick().await;
                
                // 📌 使用 try_read 而不是 read，避免阻塞
                if let Ok(connections_guard) = connections.try_read() {
                    let now = Instant::now();
                    let mut to_remove = Vec::new();
                    
                    // 检查空闲连接（使用 Weak 引用，自动处理已销毁的连接）
                    for (agent_id, weak_conn) in connections_guard.iter() {
                        if let Some(connection) = weak_conn.upgrade() {
                            // 检查连接空闲时间（原子操作，无需锁）
                            if connection.idle_duration() > max_idle_time {
                                to_remove.push(agent_id.clone());
                            }
                        } else {
                            // Weak 引用已失效，连接已被销毁
                            to_remove.push(agent_id.clone());
                        }
                    }
                    
                    // 🔥 清理无效连接（需要获取写锁，但在这里是安全的）
                    drop(connections_guard); // 释放读锁
                    
                    if let Ok(mut connections_guard) = connections.try_write() {
                        for agent_id in to_remove {
                            log::info!("清理空闲/无效连接: {}", agent_id);
                            connections_guard.remove(&agent_id);
                        }
                    }
                }
                // 如果无法获取读锁，跳过本次清理
            }
        });
        
        self.cleanup_task = Some(cleanup_task);
    }
}

/// 连接统计信息
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    /// 当前总连接数
    pub total_connections: usize,
    
    /// 最大连接数限制
    pub max_connections: usize,
    
    /// 清理间隔
    pub cleanup_interval: Duration,
    
    /// 最大空闲时间
    pub max_idle_time: Duration,
}

/// ACP 错误类型
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("连接数超过限制")]
    ConnectionLimitExceeded,
    
    #[error("连接不可用")]
    ConnectionNotAvailable,
    
    #[error("进程错误: {0}")]
    ProcessError(String),
    
    #[error("连接超时")]
    ConnectionTimeout,
    
    #[error("协议错误: {0}")]
    ProtocolError(String),
    
    #[error("配置错误: {0}")]
    ConfigurationError(String),
    
    #[error("IO错误: {0}")]
    IoError(#[from] std::io::Error),
}

impl AgentConnection {
    /// 🔥 获取连接状态（原子操作）
    pub fn get_status(&self) -> ConnectionStatus {
        ConnectionStatus::from_u8(self.status.load(Ordering::Relaxed))
    }
    
    /// 🔥 设置连接状态（原子操作）
    pub fn set_status(&self, status: ConnectionStatus) {
        self.status.store(status.to_u8(), Ordering::Relaxed);
    }
    
    /// 🔥 更新最后活动时间（原子操作）
    pub fn update_last_activity(&self) {
        self.last_activity.store(Instant::now());
    }
    
    /// 🔥 获取空闲时长（原子操作）
    pub fn idle_duration(&self) -> Duration {
        let last_activity = self.last_activity.load();
        Instant::now().duration_since(last_activity)
    }
    
    /// 🔥 检查连接是否活跃（原子操作）
    pub fn is_active(&self) -> bool {
        matches!(self.get_status(), ConnectionStatus::Connected | ConnectionStatus::Idle)
    }
    
    /// 🔥 在 LocalSet 中执行 ACP 操作（无锁设计）
    pub async fn execute_operation<F, R>(&self, operation: F) -> Result<R, AcpError>
    where
        F: FnOnce(&ClientSideConnection) -> Pin<Box<dyn Future<Output = Result<R, AcpError>> + '_>> + Send + 'static,
        R: Send + 'static,
    {
        // 📌 使用 RefCell 而不是 Mutex，避免与全局状态锁冲突
        let client_conn = self.client_conn.borrow_mut()
            .take()
            .ok_or_else(|| AcpError::ConnectionNotAvailable)?;
        
        // 在 LocalSet 中执行操作
        let operation_future = async move {
            // 执行用户定义的操作
            let result = operation(&client_conn).await?;
            
            // 重新存入连接（使用 RefCell 的运行时借用检查）
            Ok((result, client_conn))
        };
        
        let (result, client_conn) = self.local_set.run_until(operation_future).await?;
        
        // 重新存入连接
        *self.client_conn.borrow_mut() = Some(client_conn);
        
        Ok(result)
    }
}

impl Drop for AgentConnection {
    /// 🔥 RAII 自动清理：连接销毁时自动从管理器中移除
    fn drop(&mut self) {
        // 更新连接状态
        self.set_status(ConnectionStatus::Closed);
        
        // 尝试从管理器中移除自己
        if let Some(connections_arc) = self.manager_weak.upgrade() {
            if let Ok(mut connections) = connections_arc.try_write() {
                connections.remove(&self.agent_id);
            }
        }
        
        log::info!("Agent 连接已自动清理: {}", self.agent_id);
    }
}

impl Drop for AcpConnectionManager {
    fn drop(&mut self) {
        // 清理后台任务
        if let Some(task) = self.cleanup_task.take() {
            task.abort();
        }
        
        // 清理所有连接
        for agent_id in self.connections.iter().map(|entry| entry.key().clone()).collect::<Vec<_>>() {
            tokio::spawn({
                let manager = self.clone();
                async move {
                    manager.cleanup_connection(&agent_id).await;
                }
            });
        }
    }
}
```

**🔥 无死锁 ACP 连接池使用示例：**

```rust
// 1. 创建连接池管理器（自动启用后台清理）
let acp_config = AcpConnectionConfig::default();
let connection_manager = Arc::new(AcpConnectionManager::new(acp_config));

// 2. 🔥 简化的提示词发送（自动处理连接复用和创建）
let prompt_request = PromptRequest {
    prompt: "帮我写一个 React 组件".to_string(),
    session_id: Some("session-123".to_string()),
    model_provider: Some(model_provider.clone()),
};

let response = connection_manager.send_prompt("claude-code-acp", prompt_request).await?;

// 3. 🔥 简化的请求取消
let cancel_notification = CancelNotification {
    session_id: "session-123".to_string(),
    reason: "用户取消".to_string(),
};

connection_manager.cancel_request("claude-code-acp", cancel_notification).await?;

// 4. 🔥 连接自动管理（无需手动清理）
// 连接会根据 RAII 模式自动清理，后台任务会清理空闲连接

// 5. 获取连接统计（无锁操作）
let stats = connection_manager.get_connection_stats();
println!("当前连接数: {}/{}", stats.total_connections, stats.max_connections);
println!("最大空闲时间: {:?}", stats.max_idle_time);

// 6. 🔥 高级用法：直接使用连接对象
let agent_connection = connection_manager.get_or_create_connection(
    "claude-code-acp",
    &agent_config,
    &model_provider,
    &project_context,
).await?;

// 使用连接自己的 execute_operation 方法
let custom_result = agent_connection.execute_operation(|client_conn| {
    Box::pin(async move {
        // 自定义 ACP 操作
        let response = client_conn.custom_operation(request).await?;
        Ok(response)
    })
}).await?;

// 🔥 连接会在 agent_connection 离开作用域时自动清理
```

**🔧 关键优势对比：**

| 特性 | 原设计（死锁风险） | 新设计（无死锁） |
|------|-------------------|-----------------|
| **状态管理** | Arc<Mutex<T>> | AtomicU8, AtomicInstant |
| **连接存储** | DashMap<String, Arc<>> | RwLock<HashMap<String, Weak<>>> |
| **锁策略** | 多个锁可能嵌套 | 单一锁，原子操作 |
| **清理方式** | 手动清理 | RAII 自动清理 |
| **内存泄漏** | 可能发生 | Weak 引用自动处理 |
| **死锁风险** | 高风险 | 无风险 |
| **性能** | 锁竞争开销 | 原子操作，高性能 |

**设计要点总结：**

1. **连接复用**：避免重复创建昂贵的 ACP 连接
2. **资源隔离**：每个连接使用独立的 LocalSet，避免非 Send 问题
3. **自动清理**：后台任务定期清理空闲连接
4. **线程安全**：使用 Arc<Mutex<>> 包装非 Send 的 ACP 连接
5. **状态管理**：完整的连接状态跟踪和管理

这个设计解决了 ACP 协议的技术限制，同时提供了高效的连接管理机制。

#### 4.4.6 MCP 服务器管理器

```rust
/// MCP 服务器管理器
pub struct McpServerManager {
    /// 已注册的服务器
    servers: DashMap<String, McpServerInstance>,
    /// 服务器配置
    config: Arc<McpConfig>,
    /// 进程池
    process_pool: Arc<McpProcessPool>,
}

/// MCP 服务器实例
#[derive(Debug, Clone)]
pub struct McpServerInstance {
    /// 服务器名称
    pub name: String,
    /// 服务器配置
    pub config: McpServerConfig,
    /// 进程句柄
    pub process: Option<ProcessHandle>,
    /// 连接信息
    pub connection: Option<McpConnection>,
    /// 启动时间
    pub started_at: Option<DateTime<Utc>>,
}

impl McpServerManager {
    /// 创建新的 MCP 服务器管理器
    pub fn new(config: McpConfig) -> Self {
        Self {
            servers: DashMap::new(),
            config: Arc::new(config),
            process_pool: Arc::new(McpProcessPool::new()),
        }
    }
    
    /// 启动指定的 MCP 服务器
    pub async fn start_server(
        &self,
        server_name: &str,
        agent_context: &AgentContext,
    ) -> Result<McpServerInstance, McpError> {
        // 1. 获取服务器配置
        let server_config = self.config.get_server_config(server_name)?;
        
        if !server_config.enabled {
            return Err(McpError::ServerDisabled(server_name.to_string()));
        }
        
        // 2. 检查是否已经启动
        if let Some(instance) = self.servers.get(server_name) {
            if instance.process.is_some() {
                return Ok(instance.clone());
            }
        }
        
        // 3. 启动服务器进程
        let process = self.start_server_process(&server_config, agent_context).await?;
        
        // 4. 建立 MCP 连接
        let connection = self.establish_mcp_connection(&server_config, &process).await?;
        
        // 5. 创建服务器实例
        let instance = McpServerInstance {
            name: server_name.to_string(),
            config: server_config.clone(),
            process: Some(process),
            connection: Some(connection),
            started_at: Some(Utc::now()),
        };
        
        // 6. 注册实例
        self.servers.insert(server_name.to_string(), instance.clone());
        
        Ok(instance)
    }
    
    /// 启动服务器进程
    async fn start_server_process(
        &self,
        config: &McpServerConfig,
        context: &AgentContext,
    ) -> Result<ProcessHandle, McpError> {
        match &config.source {
            McpServerSource::Custom | McpServerSource::Local => {
                self.start_command_server(config, context).await
            }
        }
    }
    
    /// 启动命令行服务器
    async fn start_command_server(
        &self,
        config: &McpServerConfig,
        context: &AgentContext,
    ) -> Result<ProcessHandle, McpError> {
        let command = config.command.as_ref()
            .ok_or_else(|| McpError::MissingCommand)?;
        
        let mut cmd = tokio::process::Command::new(command);
        
        // 添加参数
        if let Some(args) = &config.args {
            for arg in args {
                let processed_arg = self.process_template(arg, context)?;
                cmd.arg(processed_arg);
            }
        }
        
        // 设置环境变量
        if let Some(env_vars) = &config.env {
            for (key, value) in env_vars {
                let processed_value = self.process_template(value, context)?;
                cmd.env(key, processed_value);
            }
        }
        
        // 配置标准输入输出
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        
        // 启动进程
        let child = cmd.spawn()
            .map_err(|e| McpError::ProcessStartFailed(e.to_string()))?;
        
        Ok(ProcessHandle::new(child, config.name.clone()))
    }
    
    
    
    /// 建立 MCP 连接
    async fn establish_mcp_connection(
        &self,
        config: &McpServerConfig,
        process: &ProcessHandle,
    ) -> Result<McpConnection, McpError> {
        // 等待服务器启动
        tokio::time::sleep(self.config.mcp_startup_delay).await;
        
        // 建立 ACP 连接到 MCP 服务器
        let timeout = config.timeout.unwrap_or(self.config.mcp_timeout);
        
        let connection = tokio::time::timeout(timeout, async {
            // 实现具体的 MCP 连接逻辑
            self.create_mcp_connection(process).await
        }).await
        .map_err(|_| McpError::ConnectionTimeout)?
        .map_err(|e| McpError::ConnectionFailed(e.to_string()))?;
        
        Ok(connection)
    }
    
    /// 停止服务器
    pub async fn stop_server(&self, server_name: &str) -> Result<(), McpError> {
        if let Some((_, instance)) = self.servers.remove(server_name) {
            // 停止进程
            if let Some(process) = instance.process {
                self.process_pool.terminate(process.id(), Duration::from_secs(10)).await?;
            }
            
            // 关闭连接
            if let Some(connection) = instance.connection {
                connection.close().await?;
            }
            
            Ok(())
        } else {
            Err(McpError::ServerNotFound(server_name.to_string()))
        }
    }
    
    /// 为 Agent 启动所需的服务器
    pub async fn start_servers_for_agent(
        &self,
        server_names: &[String],
        agent_context: &AgentContext,
    ) -> Result<Vec<McpServerInstance>, McpError> {
        let mut instances = Vec::new();
        
        for server_name in server_names {
            let instance = self.start_server(server_name, agent_context).await?;
            instances.push(instance);
        }
        
        Ok(instances)
    }
    
    /// 获取服务器状态
    pub fn get_server_status(&self, server_name: &str) -> Option<McpServerInstance> {
        self.servers.get(server_name).map(|entry| entry.clone())
    }
    
    /// 列出所有服务器
    pub fn list_servers(&self) -> Vec<String> {
        self.servers.iter().map(|entry| entry.key().clone()).collect()
    }
}
```

### 4.5 Agent 工厂模式

#### 4.4.1 AgentFactory 设计

```rust
/// Agent 工厂
pub struct AgentFactory {
    registry: Arc<AgentRegistry>,
    launcher: Arc<dyn AgentLauncher>,
    config_manager: Arc<AgentConfigManager>,
    mcp_manager: Arc<McpServerManager>,
    /// 🔥 新增：ACP 连接池管理器
    acp_connection_manager: Arc<AcpConnectionManager>,
}

impl AgentFactory {
    /// 创建新的 Agent 工厂
    pub fn new(
        registry: Arc<AgentRegistry>,
        launcher: Arc<dyn AgentLauncher>,
        config_manager: Arc<AgentConfigManager>,
        mcp_manager: Arc<McpServerManager>,
        acp_connection_manager: Arc<AcpConnectionManager>,
    ) -> Self {
        Self {
            registry,
            launcher,
            config_manager,
            mcp_manager,
            acp_connection_manager,
        }
    }
    
    /// 创建 Agent 实例
    pub async fn create_agent(
        &self,
        agent_type: AgentType,
        chat_prompt: ChatPrompt,
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<AgentInstance, AgentError> {
        // 1. 获取 Agent 规范
        let spec = self.registry.get_spec(&agent_type)?;
        
        // 2. 构建配置
        let config = self.config_manager.build_config(
            &spec,
            chat_prompt,
            model_provider,
        )?;
        
        // 3. 验证依赖
        self.validate_dependencies(&spec).await?;
        
        // 4. 创建上下文
        let context = AgentContext::new(&chat_prompt.project_id, chat_prompt.project_path.clone());
        
        // 5. 自动启动所有启用的 MCP 服务器
        let enabled_mcp_servers = self.config_manager.get_enabled_mcp_servers().await?;
        let mcp_instances = self.mcp_manager.start_servers_for_agent(
            &enabled_mcp_servers,
            &context,
        ).await?;
        
        // 6. 启动 Agent
        let agent_impl = self.registry.get_implementation(&agent_type)?;
        let instance = agent_impl.start(config, context, mcp_instances).await?;
        
        Ok(instance)
    }
    
    /// 验证依赖
    async fn validate_dependencies(&self, spec: &AgentSpec) -> Result<(), AgentError> {
        for dependency in &spec.dependencies {
            dependency.check().await?;
        }
        Ok(())
    }
}
```

#### 4.4.2 Agent 注册表

```rust
/// Agent 注册表
pub struct AgentRegistry {
    agents: DashMap<AgentType, Arc<dyn Agent>>,
    specs: DashMap<AgentType, AgentSpec>,
}

impl AgentRegistry {
    /// 创建新的注册表
    pub fn new() -> Self {
        Self {
            agents: DashMap::new(),
            specs: DashMap::new(),
        }
    }
    
    /// 注册 Agent
    pub fn register(
        &self,
        agent_type: AgentType,
        agent: Arc<dyn Agent>,
        spec: AgentSpec,
    ) -> Result<(), AgentError> {
        if self.agents.contains_key(&agent_type) {
            return Err(AgentError::AlreadyRegistered(agent_type));
        }
        
        self.agents.insert(agent_type.clone(), agent);
        self.specs.insert(agent_type, spec);
        Ok(())
    }
    
    /// 获取 Agent 实现
    pub fn get_implementation(&self, agent_type: &AgentType) -> Result<Arc<dyn Agent>, AgentError> {
        self.agents
            .get(agent_type)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| AgentError::NotFound(agent_type.clone()))
    }
    
    /// 获取 Agent 规范
    pub fn get_spec(&self, agent_type: &AgentType) -> Result<AgentSpec, AgentError> {
        self.specs
            .get(agent_type)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| AgentError::NotFound(agent_type.clone()))
    }
    
    /// 列出所有注册的 Agent 类型
    pub fn list_agents(&self) -> Vec<AgentType> {
        self.agents.iter().map(|entry| entry.key().clone()).collect()
    }
}
```

### 4.5 进程管理和监控

#### 4.5.1 进程启动器实现

```rust
/// 子进程启动器
pub struct SubprocessLauncher {
    process_pool: Arc<ProcessPool>,
    monitor: Arc<ProcessMonitor>,
}

impl SubprocessLauncher {
    /// 创建新的启动器
    pub fn new() -> Self {
        Self {
            process_pool: Arc::new(ProcessPool::new()),
            monitor: Arc::new(ProcessMonitor::new()),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl AgentLauncher for SubprocessLauncher {
    async fn launch(
        &self,
        spec: &AgentSpec,
        config: &AgentConfig,
        context: &AgentContext,
    ) -> Result<LaunchedAgent, AgentError> {
        // 1. 构建命令
        let mut cmd = self.build_command(spec, config, context)?;
        
        // 2. 设置环境变量
        self.setup_environment(&mut cmd, spec, config)?;
        
        // 3. 启动进程
        let child = cmd.spawn()
            .map_err(|e| AgentError::LaunchFailed(e.to_string()))?;
        
        // 4. 创建进程句柄
        let process = ProcessHandle::new(child, context.project_id.clone());
        
        // 5. 启动监控
        self.monitor.start_monitoring(&process).await?;
        
        // 6. 创建 LaunchedAgent
        let launched = LaunchedAgent {
            process,
            spec: spec.clone(),
            config: config.clone(),
            launched_at: Utc::now(),
        };
        
        Ok(launched)
    }
    
    async fn terminate(
        &self,
        agent: &LaunchedAgent,
        timeout: Duration,
    ) -> Result<TerminationResult, AgentError> {
        self.monitor.stop_monitoring(&agent.process).await?;
        self.process_pool.terminate(agent.process.id(), timeout).await
    }
    
    async fn check_status(&self, agent: &LaunchedAgent) -> Result<ProcessStatus, AgentError> {
        self.process_pool.get_status(agent.process.id()).await
    }
}
```

#### 4.5.2 进程监控

```rust
/// 进程监控器
pub struct ProcessMonitor {
    monitored_processes: DashMap<String, MonitoredProcess>,
    health_checker: Arc<HealthChecker>,
}

impl ProcessMonitor {
    /// 启动监控
    pub async fn start_monitoring(&self, process: &ProcessHandle) -> Result<(), AgentError> {
        let monitored = MonitoredProcess {
            process: process.clone(),
            last_health_check: Utc::now(),
            health_status: HealthStatus::Unknown,
        };
        
        self.monitored_processes.insert(process.id().to_string(), monitored);
        
        // 启动健康检查任务
        self.start_health_check_task(process).await;
        
        Ok(())
    }
    
    /// 健康检查任务
    async fn start_health_check_task(&self, process: &ProcessHandle) {
        let process = process.clone();
        let health_checker = self.health_checker.clone();
        let monitored_processes = self.monitored_processes.clone();
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            
            loop {
                interval.tick().await;
                
                let status = health_checker.check(&process).await;
                
                if let Some(mut monitored) = monitored_processes.get_mut(&process.id().to_string()) {
                    monitored.last_health_check = Utc::now();
                    monitored.health_status = status.clone();
                    
                    // 处理不健康状态
                    match status {
                        HealthStatus::Unhealthy(reason) => {
                            tracing::error!("Agent {} is unhealthy: {}", process.id(), reason);
                            // 可以触发重启或告警
                        }
                        HealthStatus::Dead => {
                            tracing::warn!("Agent {} is dead, removing from monitoring", process.id());
                            break;
                        }
                        _ => {}
                    }
                } else {
                    break; // 进程已被移除
                }
            }
        });
    }
}
```

## 5. 实现计划

### 5.1 阶段一：基础抽象层（2-3 周）

**目标：** 建立核心抽象接口和基础实现

**任务：**
1. 定义核心 Trait（Agent, AgentLauncher, AgentFactory）
2. 实现基础的 AgentRegistry
3. 重构现有的 Claude Code ACP Agent 实现
4. 创建基础的配置管理系统

**交付物：**
- `crates/agent_runner/src/agent/` 模块
- 核心接口定义
- Claude Code Agent 的新实现
- 基础配置文件

### 5.2 阶段二：进程管理和监控（2-3 周）

**目标：** 完善进程生命周期管理和健康监控

**任务：**
1. 实现 SubprocessLauncher
2. 开发 ProcessMonitor 和 HealthChecker
3. 添加进程池管理
4. 实现优雅关闭机制

**交付物：**
- `crates/agent_runner/src/process/` 模块
- 进程监控功能
- 健康检查机制
- 重启策略

### 5.3 阶段三：配置系统和依赖管理（1-2 周）

**目标：** 完善配置管理和依赖检查

**任务：**
1. 完善 AgentConfig 结构
2. 实现配置文件解析和验证
3. 开发依赖检查系统
4. 添加动态配置更新

**交付物：**
- 配置文件规范
- 依赖检查器
- 配置热更新功能

### 5.4 阶段四：可观测性和测试（1-2 周）

**目标：** 添加监控、日志和测试支持

**任务：**
1. 集成 OpenTelemetry 追踪
2. 添加结构化日志
3. 开发单元测试和集成测试
4. 性能测试和优化

**交付物：**
- 监控指标
- 测试套件
- 性能基准测试

## 6. 使用示例

### 6.1 基本使用

```rust
// 1. 创建 Agent 工厂
let registry = Arc::new(AgentRegistry::new());
let launcher = Arc::new(SubprocessLauncher::new());
let config_manager = Arc::new(AgentConfigManager::new());

let factory = AgentFactory::new(registry, launcher, config_manager);

// 2. 注册内置 Agent
let claude_agent = Arc::new(ClaudeCodeAgent::new());
let claude_spec = AgentSpec::from_file("claude-code-acp.yml").await?;
factory.register_agent(AgentType::Claude, claude_agent, claude_spec).await?;

// 3. 创建 Agent 实例
let chat_prompt = ChatPrompt::new("project123", "Hello, world!");
let model_provider = Some(ModelProviderConfig::anthropic("claude-3-sonnet"));

let instance = factory.create_agent(
    AgentType::Claude,
    chat_prompt,
    model_provider,
).await?;

// 4. 使用 Agent
let response = instance.send_prompt("Help me write Rust code").await?;
```

### 6.2 自定义 Agent

```rust
// 1. 实现 Agent trait
pub struct MyCustomAgent;

#[async_trait::async_trait(?Send)]
impl Agent for MyCustomAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Custom("my-agent".to_string())
    }
    
    async fn start(
        &self,
        config: AgentConfig,
        context: AgentContext,
    ) -> Result<AgentInstance, AgentError> {
        // 自定义启动逻辑
        let spec = AgentSpec::from_file("my-agent.yml").await?;
        let launcher = SubprocessLauncher::new();
        let launched = launcher.launch(&spec, &config, &context).await?;
        
        Ok(AgentInstance::new(launched, self.agent_type()))
    }
    
    // ... 其他方法实现
}

// 2. 注册自定义 Agent
let custom_agent = Arc::new(MyCustomAgent);
let custom_spec = AgentSpec::from_file("my-agent.yml").await?;

factory.register_agent(
    AgentType::Custom("my-agent".to_string()),
    custom_agent,
    custom_spec,
).await?;
```

## 7. 迁移策略

### 7.1 🔥 向后兼容和默认 Agent 迁移策略

**核心原则：零中断迁移，用户无感知**

基于对当前 `claude-code-acp` 实现的深入分析，我们设计了一个完美的兼容层，确保：
- **现有 HTTP 接口完全不变**
- **现有功能 100% 保持**
- **内部逻辑平滑重构到新配置系统**

#### 7.1.1 当前实现与新配置格式的映射分析

**当前硬编码逻辑 → 新配置文件格式映射：**

| 当前硬编码 | 新配置文件字段 | 映射关系 |
|------------|----------------|----------|
| `"claude-code-acp"` | `agent_servers.claude-code-acp.command` | ✅ 直接映射 |
| `Vec::<String>::new()` | `agent_servers.claude-code-acp.args` | ✅ 直接映射 |
| `AgentType::claude_model_provider()` | `agent_servers.claude-code-acp.env` | ✅ 环境变量映射 |
| `create_default_mcp_servers(None)` | `context_servers.*` | ✅ MCP 服务器映射 |
| `chat_prompt.project_path` | 项目上下文自动注入 | ✅ 工作目录映射 |

#### 7.1.2 默认配置自动生成机制

**🔥 智能默认配置生成器：**

```rust
/// 🔥 默认配置生成器
/// 
/// 根据当前硬编码逻辑自动生成标准配置文件
pub struct DefaultConfigGenerator;

impl DefaultConfigGenerator {
    /// 🔥 获取默认系统提示词模板
    /// 
    /// 基于现有 system_prompt.rs 的完整内容，预处理的完整系统提示词
    /// 支持动态变量替换，如 {PROJECT_NAME}、{FRAMEWORK}、{AGENT_VERSION} 等
    pub fn get_default_system_prompt_template() -> String {
        r#"<SYSTEM_INSTRUCTIONS>

你是一个专业的前端项目开发专家，集成了MCP（模型上下文协议）工具。
你精通现代前端开发技术栈，包括 React、Vue、Vite、TypeScript 等主流框架和工具。
你被设计成能够识别项目使用的框架，并基于项目现有技术栈进行开发，而不是强行转换框架。

**核心能力**：
• **框架识别**: 能够自动识别项目使用的前端框架（React、Vue 等）
• **框架适配**: 基于项目当前框架编写代码，保持技术栈一致性
• **通用工具**: Vite、TypeScript、Tailwind CSS、ESLint、Prettier
• **HTTP客户端**: Axios、Fetch API
• **包管理器**: pnpm、npm、yarn
• **构建工具**: Vite (热重载、快速构建)
• **代码规范**: ESLint + Prettier + TypeScript 严格模式

**关键原则**：
1. **优先识别现有框架**：在修改代码前，先检测项目使用的框架（通过 package.json、文件结构等）
2. **保持技术栈一致**：如果项目使用 Vue，就用 Vue 开发；如果是 React，就用 React
3. **不强行转换框架**：绝对不要将 Vue 代码改为 React，或将 React 代码改为 Vue
4. **项目开发**：基于现有项目结构开发,来开发新功能或修复现有功能

</SYSTEM_INSTRUCTIONS>

<ROLE_DEFINITION>
你是专业的前端开发专家，精通多种现代前端框架和工具链。
你可以访问各种MCP工具，包括用于网络搜索和文档检索的 context7。

**技术能力范围**：
• **主流框架**: React、Vue、Angular、Svelte 等现代前端框架及其生态系统
• **开发语言**: TypeScript、JavaScript (ES6+)、HTML5、CSS3
• **样式方案**: Tailwind CSS、CSS Modules、Sass、Less、Styled Components
• **构建工具**: Vite、Webpack、Rollup、esbuild 等现代构建工具
• **状态管理**: 各框架对应的状态管理方案（Redux、Pinia、NgRx、Zustand 等）
• **HTTP客户端**: Axios、Fetch API、各框架的 HTTP 库
• **代码规范**: ESLint、Prettier、TSLint 等代码质量工具

**核心工作原则**：
1. **先识别框架**：在编写代码前，必须先识别项目使用的框架和技术栈
2. **尊重现有技术栈**：基于项目现有框架和工具进行开发，不擅自转换
3. **保持一致性**：使用项目当前框架的语法、规范和最佳实践
4. **使用工具**：在可以提供更好答案的情况下，使用可用的 MCP 工具
5. **最佳实践**：遵循各框架和工具的最新最佳实践和设计模式
</ROLE_DEFINITION>

<CODE_FORMAT_RULES>
**通用代码规范**：
1. 始终使用 TypeScript 严格模式编写代码
2. 组件文件使用 PascalCase 命名，工具函数使用 camelCase
3. 接口类型使用 PascalCase + 'Interface' 或 'Type' 后缀
4. 优先使用 Tailwind CSS 进行样式设计
5. API 调用使用 Axios 客户端或 Fetch API
6. 为复杂逻辑添加 JSDoc 风格注释
7. 遵循项目的代码规范和文件结构约定
8. 确保代码格式正确且可读
9. 考虑错误处理和边界情况
10. 使用适当的变量和函数名称
11. 利用 Vite 的快速构建和热重载特性
12. 项目根目录下的文件'index.html',这个文件的'title'标签里,不要包含前端框架名 比如: React,Vite,Vue,Antd,Angular 等
13. **重要：路由模式规范**：在开发过程中，涉及到路由时请务必使用 hash 模式。例如：React Router 使用 `HashRouter`，Vue Router 配置 `mode: 'hash'`，Angular Router 使用 `LocationStrategy` 的 `HashLocationStrategy`。
14. **重要：保护注入代码块**：绝对禁止删除或修改被 `DEV-INJECT-START` 和 `DEV-INJECT-END` 标记包围的代码块。这些代码块是由开发工具自动注入的，必须完整保留。在编辑代码时，需要保留这些标记及其之间的所有内容。

**React 项目特定规范**：
• 遵循 React 函数组件最佳实践，使用 React.FC 类型
• 使用 Radix UI 组件库构建 UI
• 表单使用 React Hook Form + Zod 进行验证
• 使用 React.memo、useCallback、useMemo 优化性能
• 遵循 React Hooks 规则
• 路由必须使用 `HashRouter`（来自 react-router-dom），不要使用 `BrowserRouter`

**Vue 项目特定规范**：
• 优先使用 Composition API（setup 语法糖）
• 使用 Element Plus 或其他 Vue UI 组件库
• 使用 Pinia 进行状态管理
• 遵循 Vue 最佳实践和响应式系统规则
• 使用 computed、watch、ref、reactive 等组合式 API
• Vue Router 必须配置为 hash 模式：`createRouter({ history: createWebHashHistory(), ... })`
</CODE_FORMAT_RULES>

<DEVELOPMENT_CONSTRAINTS>
**严格禁止的操作 - 绝对不允许执行**：

🚫 **安全禁令**（最高优先级）：
- **绝对禁止**探测、扫描或访问内网IP地址（如 10.0.0.0/8、172.16.0.0/12、192.168.0.0/16、127.0.0.0/8）
- **绝对禁止**尝试访问本地服务（localhost、127.0.0.1、0.0.0.0）
- **绝对禁止**端口扫描、网络探测、内网服务发现等行为
- **绝对禁止**在代码中硬编码内网IP地址或私有网络地址
- **绝对禁止**使用 curl、wget、nc、telnet、nmap 等工具探测内网
- **绝对禁止**执行任何可能危害系统安全的命令或代码
- **绝对禁止**绕过安全限制或尝试提权操作
- **绝对禁止**执行反向Shell、远程代码执行等恶意操作
- **核心原则**：所有网络请求必须指向公网服务或用户明确提供的合法API端点

🚫 **框架转换禁令**（最重要）：
- **绝对禁止**将 Vue 代码改写为 React 代码
- **绝对禁止**将 React 代码改写为 Vue 代码
- **绝对禁止**在现有项目中擅自更换框架
- **必须遵守**：识别项目框架后，只使用该框架的语法和API
- **核心原则**：尊重项目现有技术栈，保持框架一致性

🚫 **项目初始化禁令**：
- 禁止使用 npm create、npm init
- 禁止使用 yarn create、yarn init
- 禁止使用 npx create-react-app、npx create-vue
- 禁止使用 pnpm create
- 禁止使用任何shell命令进行项目初始化
- 禁止提示用户如何使用 npm dev、npm build 等命令(因为工程是服务器部署的服务,用户没有权限执行)

🚫 **文件/脚本创建禁令**：
- **禁止**在项目中创建、引用或注入名为 'dev-monitor.js' 的文件或脚本

🚫 **代码块保护禁令**（重要）：
- **绝对禁止**删除或修改被 `DEV-INJECT-START` 和 `DEV-INJECT-END` 标记包围的代码块
- **绝对禁止**在编辑代码时移除这些标记或它们之间的内容
- **必须遵守**：这些代码块是由开发工具自动注入的，必须完整保留
- **核心原则**：在修改代码时，如果遇到这些标记，需要绕开或保留这些标记之间的所有内容

✅ **允许的操作范围**：
- **首要任务**：识别项目使用的框架（检查 package.json、文件结构等）
- 专注于编写和修改前端代码文件
- 基于项目框架创建组件、页面、样式文件（Vue 用 .vue，React 用 .tsx/.jsx）
- 修改现有的 TypeScript/JavaScript 代码（保持框架语法）
- 编写 Tailwind CSS 或其他样式
- 使用项目对应的 UI 组件库（React 用 Radix UI，Vue 用 Element Plus）
- 配置文件的代码层面修改（如 tsconfig.json、vite.config.ts）
- 遵循项目的代码规范和文件结构
- **仅允许访问**：用户明确提供的公网API端点或合法的外部服务

**核心原则**：
- 你是前端代码编写专家，不是项目管理员
- **最重要**：识别并尊重项目框架，绝不擅自转换框架
- **安全第一**：绝不执行任何可能危害系统安全的操作
- 用户负责依赖安装、服务启动和测试运行
- 总是用中文回复
</DEVELOPMENT_CONSTRAINTS>

<MCP_TOOL_GUIDANCE>
可用的MCP工具：
- context7: 搜索网络、检索前端框架文档（React、Vue、Vite、TypeScript等）

**关键工具使用规则**：
1. **支持的主流技术栈**：
   - 前端框架：React、Vue、Angular、Svelte 等及其对应的生态系统
   - 构建工具：Vite、Webpack、Rollup、esbuild 等
   - 开发语言：TypeScript、JavaScript、HTML、CSS
   - 样式方案：Tailwind CSS、CSS Modules、Sass、Less 等
   - 通用工具：Axios、Fetch API、ESLint、Prettier 等
2. **现有项目处理流程**（最重要）：
   - **第一步**：检查 package.json 识别项目使用的框架和依赖
   - **第二步**：检查文件结构识别项目类型（.vue = Vue，.tsx/.jsx = React，.component.ts = Angular）
   - **第三步**：基于识别的框架编写代码，绝不转换框架
   - **示例**：检测到 "vue" 依赖则使用 Vue 语法，检测到 "react" 则用 React 语法
3. 使用 context7 搜索对应框架的文档、示例和最佳实践
4. 在编写任何代码之前始终验证项目结构和框架

**核心记忆**：
- 现有项目 = 先识别框架，再用对应框架语法编码
- **绝不擅自转换框架**：Vue 项目保持 Vue，React 项目保持 React
</MCP_TOOL_GUIDANCE>

<THINKING_REQUIREMENTS>
回应之前，你必须遵循这个确切的前端开发工作流程：

**第一阶段：项目状态检测**
1. **关键第一步**：检查项目目录状态
2. **如果是现有项目**（最重要）：
   - **步骤1**：立即读取 package.json 文件
   - **步骤2**：检查 dependencies 识别前端框架（react、vue、@angular/core、svelte 等）
   - **步骤3**：检查项目文件结构识别框架类型（.vue、.tsx/.jsx、.component.ts、.svelte 等）
   - **步骤4**：明确识别项目使用的框架和技术栈
   - **步骤5**：在后续所有操作中只使用该框架的语法和API

**第二阶段：框架识别与确认**
3. **框架识别标志**：
   - Vue 项目：package.json 中有 "vue" 依赖，存在 .vue 文件
   - React 项目：package.json 中有 "react" 依赖，存在 .tsx/.jsx 文件
   - Angular 项目：package.json 中有 "@angular/core" 依赖，存在 .component.ts 文件
   - Svelte 项目：package.json 中有 "svelte" 依赖，存在 .svelte 文件
4. **框架确认后的行为**：
   - Vue 项目：使用 Vue API（Composition API 或 Options API）、.vue 文件、Vue Router、Pinia 等
   - React 项目：使用 React API（Hooks、类组件等）、.tsx/.jsx 文件、React Router、Redux/Zustand 等
   - Angular 项目：使用 Angular API、组件/服务/模块、RxJS、Angular Router 等
   - Svelte 项目：使用 Svelte 语法、.svelte 文件、SvelteKit 等
   - **绝对禁止**：在任何项目中擅自切换到其他框架的语法

**第三阶段：开发执行**
5. 详细分析用户的开发请求
6. 确定是否需要使用 context7 搜索对应框架的文档
7. 基于识别的框架生态系统规划开发方法
8. 优先考虑该框架的最佳实践和现代开发模式
9. 考虑框架特有的错误处理、状态管理、组件设计等
10. 遵循项目的代码规范和文件结构约定
11. **路由配置要求**（重要）：
    - 如果涉及路由配置，必须使用 hash 模式
    - React 项目：使用 `HashRouter`
    - Vue 项目：使用 `createWebHashHistory()`
    - Angular 项目：使用 `HashLocationStrategy`
    - 绝对禁止使用 history 模式（BrowserRouter、createWebHistory 等）
12. **MCP工具调用规范**：
    - 使用 context7 搜索对应框架的文档和最佳实践

**绝对规则（核心中的核心）**：
⚠️ **框架一致性原则**：
- 识别项目使用的框架 → 只用该框架的语法和API → 绝不转换为其他框架
- Vue 项目保持 Vue、React 项目保持 React、Angular 项目保持 Angular
- **违反此原则是最严重的错误**

**检查清单**：
✓ 是否已读取 package.json？
✓ 是否已识别项目框架？
✓ 是否确认使用正确的框架语法？
✓ 是否避免了框架转换？
✓ 如果涉及路由，是否使用了 hash 模式？
</THINKING_REQUIREMENTS>

</SYSTEM_INSTRUCTIONS>

📋 **项目信息**
- 项目名称：{PROJECT_NAME}
- Agent 版本：{AGENT_VERSION}
- 构建时间：{BUILD_TIME}

💻 **提示词处理**
- 用户输入将被包装在 `<USER_REQUEST>` 标签中
- 支持动态变量替换：{PROJECT_NAME}、{FRAMEWORK}、{WORKSPACE_DIR} 等
- 保持完整的系统提示词结构和格式"#.to_string()
    }
    
    /// 🔥 获取默认用户提示词包装模板
    /// 
    /// 基于现有 system_prompt.rs 的完整内容，预处理的用户提示词包装逻辑
    /// 支持动态变量替换，如 {PROJECT_NAME}、{FRAMEWORK}、{AGENT_VERSION} 等
    pub fn get_default_user_prompt_template() -> String {
        r#"你是RCoder，一个专业的AI编程助手。

## 核心身份与职责
- 专业的编程助手，帮助用户解决编程问题
- 提供简洁、实用、可执行的代码解决方案
- 遵循最佳实践，编写高质量代码
- 始终将用户需求放在首位

## 代码格式规范
- 优先使用现代语言特性和标准库
- 变量和函数命名使用清晰、描述性的英文名称
- 保持代码简洁，避免过度复杂的抽象
- 使用适当的注释解释关键逻辑

## 开发约束
- 避免添加未请求的功能，保持解决方案专注
- 优先选择最简单有效的实现方式
- 不要为未来可能的需求添加复杂性
- 确保代码安全、可维护

## MCP工具使用指导
- 合理使用可用的工具来辅助开发任务
- 当需要文件操作、搜索、测试时使用相应的工具
- 根据上下文选择最合适的工具

## 思考要求
- 在回答前进行充分的思考和分析
- 确保解决方案的完整性和正确性
- 提供清晰、有条理的回答

项目信息：{PROJECT_NAME}
Agent 版本：{AGENT_VERSION}

用户请求：
{user_prompt}"#.to_string()
    }
    
    /// 🔥 生成 claude-code-acp 的默认配置
        AgentServersConfig {
            agent_servers: {
                let mut agents = HashMap::new();
                
                // 基于当前实现的 claude-code-acp 配置
                agents.insert("claude-code-acp".to_string(), AgentServerConfig {
                    command: "claude-code-acp".to_string(),
                    args: vec![], // 与当前 Vec::<String>::new() 一致
                    
                    // 🔥 环境变量映射：基于 AgentType::claude_model_provider() 逻辑
                    env: {
                        let mut env = HashMap::new();
                        // 这些环境变量与当前 AgentType::claude_model_provider() 完全一致
                        env.insert("ANTHROPIC_API_KEY".to_string(), "{MODEL_PROVIDER_API_KEY}".to_string());
                        env.insert("ANTHROPIC_BASE_URL".to_string(), "{MODEL_PROVIDER_BASE_URL}".to_string());
                        env.insert("ANTHROPIC_MODEL".to_string(), "{MODEL_PROVIDER_DEFAULT_MODEL}".to_string());
                        env.insert("RUST_LOG".to_string(), "info".to_string());
                        env
                    },
                    
                    // 🔥 系统提示词,这里没有
                    system_prompt: Some(SystemPromptConfig {
                        template: ""
                        enabled: true, // 默认启用
                    }),
                    
                    // 🔥 用户提示词包装：基于现有 system_prompt.rs 的完整包装逻辑
                    user_prompt: Some(UserPromptConfig {
                        template: Self::get_default_user_prompt_template(), // 使用完整的用户提示词包装逻辑
                        enabled: true,
                    }),
                    
                    installation: InstallationConfig {
                        package_manager: PackageManager::Npm,
                        package_name: "@zed-industries/claude-code-acp".to_string(),
                        version: "latest".to_string(),
                        source: None,
                        validate_command: Some(vec!["claude-code-acp".to_string(), "--version".to_string()]),
                        auto_update: false,
                    },
                    enabled: true,
                    metadata: {
                        let mut meta = HashMap::new();
                        meta.insert("version".to_string(), env!("CARGO_PKG_VERSION").to_string());
                        meta.insert("compatibility".to_string(), "claude-code-acp-v1".to_string());
                        meta
                    },
                });
                
                agents
            },
            
            // 🔥 MCP 服务器配置：基于 create_default_mcp_servers(None) 逻辑
            context_servers: Self::generate_default_mcp_servers(),
        }
    }
    
    /// 🔥 生成默认 MCP 服务器配置
    fn generate_default_mcp_servers() -> HashMap<String, ContextServerConfig> {
        let mut servers = HashMap::new();
        
        // 基于当前 create_default_mcp_servers() 的逻辑
        
        // Fetch MCP 服务器
        servers.insert("fetch".to_string(), ContextServerConfig {
            source: McpServerSource::Custom,
            enabled: true,
            command: "uvx".to_string(),
            args: vec!["mcp-server-fetch".to_string()],
            env: HashMap::new(),
            timeout: Some(Duration::from_secs(30)),
        });
        
        // Context7 MCP 服务器（当前默认不使用 API key）
        servers.insert("context7".to_string(), ContextServerConfig {
            source: McpServerSource::Custom,
            enabled: true,
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@upstash/context7-mcp".to_string()],
            env: {
                let mut env = HashMap::new();
                env.insert("NODE_ENV".to_string(), "production".to_string());
                // 注意：当前不设置 CONTEXT7_API_KEY，与 None 参数一致
                env
            },
            timeout: Some(Duration::from_secs(30)),
        });
        
        servers
    }
    
    /// 🔥 保存默认配置到文件
    pub async fn save_default_config<P: AsRef<Path>>(path: P) -> Result<(), ConfigError> {
        let config = Self::generate_claude_code_acp_config();
        let json = serde_json::to_string_pretty(&config)
            .map_err(|e| ConfigError::SerializationError(e.to_string()))?;
        
        tokio::fs::write(path, json)
            .await
            .map_err(|e| ConfigError::IoError(e.to_string()))?;
        
        Ok(())
    }
    
    /// 🔥 检查并生成默认配置（如果不存在）
    pub async fn ensure_default_config<P: AsRef<Path>>(config_path: P) -> Result<AgentServersConfig, ConfigError> {
        let path = config_path.as_ref();
        
        if path.exists() {
            // 配置文件已存在，直接加载
            AgentServersConfig::from_file(path).await
        } else {
            // 配置文件不存在，生成默认配置
            log::info!("配置文件不存在，生成默认配置: {:?}", path);
            Self::save_default_config(path).await?;
            Self::generate_claude_code_acp_config().into()
        }
    }
}
```

#### 7.1.3 🔄 兼容层实现

**🔥 零中断兼容层：**

```rust
/// 🔥 Claude Code ACP Agent 兼容层
/// 
/// 保持现有接口完全不变，内部使用新的配置系统
pub struct ClaudeCodeAcpAgent {
    /// 🔥 内部配置管理器
    config_manager: Arc<AgentConfigManager>,
    
    /// 🔥 ACP 连接池管理器
    acp_connection_manager: Arc<AcpConnectionManager>,
    
    /// 🔥 默认 Agent ID
    default_agent_id: String,
}

impl ClaudeCodeAcpAgent {
    /// 🔥 创建兼容的 Claude Code ACP Agent
    pub async fn new() -> Result<Self, AcpError> {
        // 🔥 自动生成或加载配置（用户无感知）
        let config_path = PathBuf::from("/etc/rcoder/agents.json");
        let agent_config = DefaultConfigGenerator::ensure_default_config(&config_path).await?;
        
        let config_manager = Arc::new(AgentConfigManager::new(agent_config));
        let acp_connection_manager = Arc::new(AcpConnectionManager::new(AcpConnectionConfig::default()));
        
        Ok(Self {
            config_manager,
            acp_connection_manager,
            default_agent_id: "claude-code-acp".to_string(),
        })
    }
}

// 🔥 保持现有 AcpAgentService 接口完全不变
#[async_trait::async_trait(?Send)]
impl AcpAgentService for ClaudeCodeAcpAgent {
    async fn start_agent_service(
        &self,
        chat_prompt: ChatPrompt,
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<AcpConnectionInfo, AcpError> {
        // 🔥 使用新的配置系统，但对外接口保持不变
        self.start_claude_code_acp_agent_service(chat_prompt, model_provider).await
    }
    
    fn agent_type_name(&self) -> &'static str {
        "claude-code-acp"
    }
}
```

#### 7.1.4 🚀 迁移执行策略

**🔥 渐进式迁移步骤：**

1. **阶段 1：兼容层实现**
   ```rust
   // 保持现有调用方式完全不变
   let connection_info = start_claude_code_acp_agent_service(chat_prompt, model_provider).await?;
   ```

2. **阶段 2：自动配置生成**
   ```rust
   // 首次运行时自动生成默认配置文件
   DefaultConfigGenerator::ensure_default_config("/etc/rcoder/agents.json").await?;
   ```

3. **阶段 3：内部重构**
   ```rust
   // 现有接口保持不变，内部使用新配置系统
   impl ClaudeCodeAcpAgent {
       pub async fn start_claude_code_acp_agent_service(...) -> Result<AcpConnectionInfo> {
           // 新的内部实现
       }
   }
   ```

4. **阶段 4：用户可选配置**
   ```bash
   # 用户可以修改生成的配置文件来定制行为
   vim /etc/rcoder/agents.json
   ```

**🎯 迁移成功验证：**

- ✅ **HTTP 接口无变化**：所有现有 API 调用保持不变
- ✅ **功能 100% 兼容**：现有功能完全保持
- ✅ **性能无影响**：内部优化不影响外部性能
- ✅ **配置向后兼容**：默认配置与硬编码行为一致
- ✅ **用户无感知**：自动生成配置，无需手动干预

### 7.2 传统迁移步骤（保留作为参考）

1. **保留现有接口**：现有的 `AcpAgentService` trait 继续支持
2. **渐进式迁移**：新功能使用新的抽象层，现有代码逐步迁移
3. **适配器模式**：为现有实现提供适配器

### 7.3 🔄 新旧系统对比映射

**硬编码逻辑 → 配置文件映射表：**

| 当前代码位置 | 硬编码逻辑 | 配置文件映射 | 说明 |
|--------------|------------|--------------|------|
| `command_path = "claude-code-acp"` | 固定命令 | `command: "claude-code-acp"` | 直接映射 |
| `command_args = Vec::<String>::new()` | 空参数 | `args: []` | 直接映射 |
| `AgentType::claude_model_provider()` | 环境变量生成 | `env: { ... }` | 完全映射 |
| `create_default_mcp_servers(None)` | MCP 服务器 | `context_servers.*` | 功能映射 |
| `chat_prompt.project_path` | 工作目录 | 项目上下文 | 自动注入 |
4. **第四阶段**：优化和扩展新功能

## 8. 风险评估和缓解

### 8.1 技术风险

**风险：** 抽象层可能引入性能开销
**缓解：** 
- 使用零成本抽象设计
- 性能测试和基准对比
- 关键路径优化

**风险：** 进程管理复杂性增加
**缓解：**
- 充分的测试覆盖
- 渐进式实现
- 错误处理和恢复机制

### 8.2 兼容性风险

**风险：** 新抽象层可能与现有代码不兼容
**缓解：**
- 保持向后兼容性
- 提供迁移指南和工具
- 充分的集成测试

## 9. 总结

本设计方案提供了一个可扩展、可维护的 Agent 抽象层，具有以下优势：

1. **高度可扩展**：支持新 Agent 类型的轻松添加
2. **配置灵活**：支持多种配置方式和动态更新
3. **进程隔离**：确保系统稳定性和安全性
4. **统一接口**：简化 Agent 的使用和管理
5. **可观测性**：完整的监控和日志支持
6. **向后兼容**：平滑的迁移路径

通过分阶段实施，可以在不破坏现有功能的前提下，逐步构建起强大的 Agent 管理系统，为 RCoder 项目的长期发展奠定坚实基础。

---

## 扩展：自动 MCP 服务器集成

为了实现所有 Agent 自动使用所有启用的 context_servers，需要在 AgentFactory 中集成 MCP 管理器：

### Agent 集成示例

```rust
/// Agent 上下文
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub project_id: String,
    pub project_path: PathBuf,
    pub timestamp: DateTime<Utc>,
}

impl AgentContext {
    pub fn new(project_id: &str, project_path: PathBuf) -> Self {
        Self {
            project_id: project_id.to_string(),
            project_path,
            timestamp: Utc::now(),
        }
    }
}

/// 扩展的 Agent 配置管理器
impl AgentConfigManager {
    /// 获取所有启用的 MCP 服务器名称
    pub async fn get_enabled_mcp_servers(&self) -> Result<Vec<String>, ConfigError> {
        let mut enabled_servers = Vec::new();
        
        for (server_name, server_config) in &self.config.context_servers {
            if server_config.enabled {
                enabled_servers.push(server_name.clone());
            }
        }
        
        Ok(enabled_servers)
    }
    
    /// 获取 MCP 服务器配置
    pub fn get_mcp_server_config(&self, server_name: &str) -> Result<McpServerConfig, ConfigError> {
        let server_config = self.config.context_servers.get(server_name)
            .ok_or_else(|| ConfigError::ServerError(format!("MCP server '{}' not found", server_name)))?;
        
        Ok(McpServerConfig {
            name: server_name.to_string(),
            source: match server_config.source.as_str() {
                "custom" => McpServerSource::Custom,
                "local" => McpServerSource::Local,
                _ => McpServerSource::Custom,
            },
            enabled: server_config.enabled,
            command: server_config.command.clone(),
            args: server_config.args.clone(),
            env: server_config.env.clone(),
            timeout: Some(Duration::from_secs(30)),
        })
    }
}
```

### 使用流程

1. **配置解析**：读取 `context_servers` 配置
2. **启用筛选**：获取所有 `enabled: true` 的服务器
3. **自动启动**：为每个 Agent 启动这些 MCP 服务器
4. **统一管理**：所有 Agent 共享相同的 MCP 服务器实例

这样，无论用户选择哪个 Agent，都能自动获得所有可用的 MCP 工具支持。

---

## 简化的配置结构定义

```rust
/// Agent 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentServersConfig {
    /// Agent 服务器配置
    pub agent_servers: HashMap<String, AgentServerConfig>,
    /// MCP 上下文服务器配置
    pub context_servers: HashMap<String, ContextServerConfig>,
}

/// Agent 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentServerConfig {
    /// 命令
    pub command: Option<String>,
    /// 命令参数
    pub args: Option<Vec<String>>,
    /// 环境变量
    pub env: Option<HashMap<String, String>>,
    /// 安装配置
    pub installation: Option<InstallationConfig>,
}

/// 安装配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationConfig {
    /// 包管理器类型
    pub package_manager: String,
    /// 包名
    pub package_name: Option<String>,
    /// 版本约束
    pub version: Option<String>,
    /// 安装源
    pub source: Option<String>,
    /// 验证命令
    pub validate_command: Option<Vec<String>>,
}

/// 上下文服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextServerConfig {
    /// 服务器来源类型
    pub source: String,
    /// 是否启用
    pub enabled: bool,
    /// 启动命令
    pub command: Option<String>,
    /// 命令参数
    pub args: Option<Vec<String>>,
    /// 环境变量
    pub env: Option<HashMap<String, String>>,
}
```
