# Agent 抽象层设计方案

## 1. 概述

本文档设计了 RCoder 项目的 Agent 抽象层，旨在实现对不同 AI Agent 的统一管理和扩展支持。当前系统主要使用 "claude-code-acp" 作为 Agent 实现，未来需要支持更多类型的 Agent，因此需要建立一个通用的抽象层。

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

/// 🔥 新增：系统提示词配置
/// 
/// 系统提示词配置，支持模板变量和启用控制
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPromptConfig {
    /// 系统提示词模板内容
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
    /// 自定义命令行工具
    Custom,
    /// 内置扩展
    Extension,
    /// 本地可执行文件
    Local,
    /// NPM 包
    Npm,
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
| `{MODEL_PROVIDER_DEFAULT_MODEL}` | `default_model` | 默认模型 | "claude-3-5-sonnet-20241022" |
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
        "template": "你好 {user_prompt}，请帮我分析这个问题并提供详细的解决方案。"
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

**完整使用示例：**

```rust
// 1. 加载配置（由调用方负责）
let agent_config = AgentServersConfig::from_file("/etc/rcoder/agents.json").await?;
let env_resolver = EnvironmentVariableResolver::with_standard_mappings();

// 2. 准备 ModelProvider 配置
let model_provider = ModelProviderConfig {
    id: "anthropic-claude".to_string(),
    name: "Claude".to_string(),
    base_url: "https://api.anthropic.com".to_string(),
    api_key: "sk-ant-xxx".to_string(),
    requires_openai_auth: false,
    default_model: "claude-3-5-sonnet-20241022".to_string(),
    api_protocol: Some("anthropic".to_string()),
};

// 3. 准备项目上下文
let project_context = ProjectContext {
    project_id: "project-123".to_string(),
    project_name: "my-react-app".to_string(),
    project_path: PathBuf::from("/workspace/project-123"),
};

// 4. 初始化 Agent 管理器
let mut agent_manager = AgentManager::new(agent_config, env_resolver)?;

// 5. 启动 Agent（根据配置中的 agent_type 自动选择启动方式）
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
        "template": "你好，我是 {PROJECT_NAME} 项目的开发者。{user_prompt} 请提供详细的解决方案和代码示例。"
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

#### 4.4.5 MCP 服务器管理器

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
            McpServerSource::Custom | McpServerSource::Local | McpServerSource::Npm => {
                self.start_command_server(config, context).await
            }
            McpServerSource::Extension => {
                self.start_extension_server(config, context).await
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
    
    /// 启动扩展服务器
    async fn start_extension_server(
        &self,
        config: &McpServerConfig,
        _context: &AgentContext,
    ) -> Result<ProcessHandle, McpError> {
        // 内置扩展服务器的启动逻辑
        match config.name.as_str() {
            "mcp-server-context7" => {
                // 启动内置的 Context7 MCP 服务器
                self.start_builtin_context7_server(config).await
            }
            _ => Err(McpError::UnknownExtension(config.name.clone()))
        }
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
}

impl AgentFactory {
    /// 创建新的 Agent 工厂
    pub fn new(
        registry: Arc<AgentRegistry>,
        launcher: Arc<dyn AgentLauncher>,
        config_manager: Arc<AgentConfigManager>,
        mcp_manager: Arc<McpServerManager>,
    ) -> Self {
        Self {
            registry,
            launcher,
            config_manager,
            mcp_manager,
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

### 7.1 向后兼容性

1. **保留现有接口**：现有的 `AcpAgentService` trait 继续支持
2. **渐进式迁移**：新功能使用新的抽象层，现有代码逐步迁移
3. **适配器模式**：为现有实现提供适配器

### 7.2 迁移步骤

1. **第一阶段**：在不破坏现有功能的前提下，添加新的抽象层
2. **第二阶段**：将现有实现重构为新抽象层的形式
3. **第三阶段**：移除旧的实现代码
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
                "extension" => McpServerSource::Extension,
                "local" => McpServerSource::Local,
                "npm" => McpServerSource::Npm,
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
