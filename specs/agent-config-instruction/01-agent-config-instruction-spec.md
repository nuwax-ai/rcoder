# Agent 配置入参功能实现计划

## 1. 需求概述

### 1.1 功能目标
为 `/chat` 接口增加三个可选配置入参，允许用户自定义 Agent 的提示词和配置：

| 参数 | 类型 | 说明 |
|------|------|------|
| `system_prompt` | `Option<String>` | 系统提示词，可选 |
| `user_prompt` | `Option<String>` | 用户提示词模板，可选 |
| `agent_config` | `Option<ChatAgentConfig>` | Agent 运行时配置（MCP 服务器等），可选 |

### 1.2 设计原则：职责分离，无冲突

**提示词由独立入参控制，`agent_config` 只负责运行时配置**：

- `system_prompt`：直接传入系统提示词文本
- `user_prompt`：直接传入用户提示词模板（支持 `{user_prompt}` 变量）
- `agent_config`：只包含 MCP 服务器等运行时配置，**不包含提示词配置**

这样设计的优势：
1. **消除歧义**：用户不会困惑"到底用哪个提示词"
2. **简化代码**：不需要复杂的优先级合并逻辑
3. **职责清晰**：提示词归提示词，配置归配置

### 1.3 变量注入
`user_prompt` 支持模板变量 `{user_prompt}`，运行时用 `/chat` 的 `prompt` 字段值替换

### 1.4 配置生效逻辑

```
用户入参                              内部组装
───────────────────────────────────────────────────────────────────
system_prompt ────────────────────────→ AgentStartConfig.system_prompt
user_prompt ──────────────────────────→ 模板替换后的最终 prompt
agent_config.agent_server ────────────→ AgentConfig (合并覆盖默认配置)
agent_config.context_servers ─────────→ AgentStartConfig.mcp_servers

（如果入参为空，使用默认配置）
```

---

## 2. 新增数据结构定义

### 2.1 结构对比：内部配置 vs 外部入参

| 内部配置 (AgentServersConfig) | 外部入参 (ChatAgentConfig) | 说明 |
|-------------------------------|----------------------------|------|
| `agent_servers: HashMap<String, AgentConfig>` | `agent_server: Option<ChatAgentServerConfig>` | 内部支持多 Agent，外部只需单个 |
| `context_servers: HashMap<String, ContextServerConfig>` | `context_servers: HashMap<String, ChatContextServerConfig>` | MCP 工具可以有多个，保持一致 |

### 2.2 ChatAgentConfig 结构体

**位置**: `crates/shared_types/src/chat_agent_config.rs` (新文件)

```rust
//! Chat 接口专用的 Agent 配置结构体
//!
//! 简化版本，只包含运行时配置，不包含提示词配置。
//! 提示词由独立的 system_prompt 和 user_prompt 入参控制。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;

/// Chat 接口的 Agent 配置
///
/// 包含单个 Agent 的运行时配置和多个 MCP 服务器配置。
/// 提示词由独立入参 (system_prompt, user_prompt) 控制，不在此结构中。
#[derive(Debug, Clone, Serialize, Deserialize, Default, ToSchema)]
pub struct ChatAgentConfig {
    /// 单个 Agent 服务器配置（可选）
    ///
    /// 用于覆盖默认的 Agent 执行命令、参数、环境变量等。
    /// 如果不传，使用内部默认配置 (claude-code-acp)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_server: Option<ChatAgentServerConfig>,

    /// MCP 服务器配置（Context Servers）
    ///
    /// 可配置多个 MCP 工具服务器。
    /// 如果不传，使用内部默认的 MCP 配置。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub context_servers: HashMap<String, ChatContextServerConfig>,
}

/// 单个 Agent 服务器配置
///
/// 对应内部 AgentConfig 的简化版本，只暴露必要的运行时配置。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatAgentServerConfig {
    /// Agent 标识符（可选，默认使用 "claude-code-acp"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    /// 执行命令（如 "claude-code-acp", "custom-agent"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// 命令参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    /// 环境变量
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,

    /// 元数据（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

/// MCP 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatContextServerConfig {
    /// 服务器来源类型: "custom" 或 "local"
    #[serde(default = "default_custom")]
    pub source: String,

    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 执行命令 (如 "bunx", "uvx", "npx")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// 命令参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    /// 环境变量
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

fn default_custom() -> String {
    "custom".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for ChatAgentServerConfig {
    fn default() -> Self {
        Self {
            agent_id: None,
            command: None,
            args: None,
            env: None,
            metadata: None,
        }
    }
}

impl Default for ChatContextServerConfig {
    fn default() -> Self {
        Self {
            source: "custom".to_string(),
            enabled: true,
            command: None,
            args: None,
            env: None,
        }
    }
}

impl ChatAgentConfig {
    /// 检查是否有 Agent 服务器配置
    pub fn has_agent_server(&self) -> bool {
        self.agent_server.is_some()
    }

    /// 检查是否有 MCP 服务器配置
    pub fn has_context_servers(&self) -> bool {
        !self.context_servers.is_empty()
    }

    /// 获取启用的 MCP 服务器
    pub fn get_enabled_context_servers(&self) -> HashMap<String, &ChatContextServerConfig> {
        self.context_servers
            .iter()
            .filter(|(_, config)| config.enabled)
            .map(|(name, config)| (name.clone(), config))
            .collect()
    }
}

impl ChatAgentServerConfig {
    /// 获取 Agent ID，默认返回 "claude-code-acp"
    pub fn get_agent_id(&self) -> &str {
        self.agent_id.as_deref().unwrap_or("claude-code-acp")
    }
}
```

### 2.3 JSON 请求示例（完整配置）

```json
{
  "prompt": "帮我写一个 Hello World",
  "project_id": "my_project",
  "system_prompt": "你是一个专业的 Rust 开发者，擅长编写高性能代码",
  "user_prompt": "请用 Rust 语言完成以下需求：{user_prompt}",
  "agent_config": {
    "agent_server": {
      "agent_id": "claude-code-acp",
      "command": "claude-code-acp",
      "args": [],
      "env": {
        "ANTHROPIC_MODEL": "claude-sonnet-4-20250514",
        "RUST_LOG": "debug"
      }
    },
    "context_servers": {
      "context7": {
        "source": "custom",
        "enabled": true,
        "command": "bunx",
        "args": ["-y", "@upstash/context7-mcp"]
      },
      "fetch": {
        "source": "custom",
        "enabled": true,
        "command": "uvx",
        "args": ["mcp-server-fetch"]
      }
    }
  }
}
```

### 2.4 JSON 请求示例（只配置 MCP）

```json
{
  "prompt": "帮我写一个 Hello World",
  "system_prompt": "你是 Rust 专家",
  "agent_config": {
    "context_servers": {
      "context7": {
        "enabled": true,
        "command": "bunx",
        "args": ["-y", "@upstash/context7-mcp"]
      }
    }
  }
}
```

### 2.5 最简请求示例

```json
{
  "prompt": "帮我写一个 Hello World",
  "system_prompt": "你是 Rust 专家"
}
```

> **说明**: 不传 `agent_config` 时，使用默认的 Agent 和 MCP 服务器配置。

---

## 3. 现有架构分析

### 3.1 数据流路径
```
HTTP /chat (rcoder)
    ↓
gRPC ChatRequest (shared_types)
    ↓
agent_runner gRPC AgentServiceImpl
    ↓
ChatPromptBuilder → PromptMessage
    ↓
AcpAgentWorker → AcpSessionManager
    ↓
ClaudeCodeLauncher (agent_abstraction)
    ↓
NewSessionRequest._meta.systemPrompt (ACP协议)
```

### 3.2 关键文件和结构体

| 文件 | 结构体/模块 | 作用 |
|------|-------------|------|
| `crates/rcoder/src/handler/chat_handler.rs` | `ChatRequest` | HTTP 请求结构体 |
| `crates/shared_types/proto/agent.proto` | `ChatRequest` | gRPC 请求定义 |
| `crates/agent_runner/src/grpc/agent_service_impl.rs` | `AgentServiceImpl::chat()` | gRPC 服务实现 |
| `crates/agent_config/src/config/servers_config.rs` | `AgentServersConfig` | Agent 配置集合 |
| `crates/agent_abstraction/src/traits/agent.rs` | `AgentStartConfig` | Agent 启动配置 |
| `crates/agent_abstraction/src/compat/claude_code_launcher.rs` | `ClaudeCodeLauncher` | Agent 启动器 |

### 3.3 现有默认配置结构

**AgentServersConfig** (默认 JSON 配置):
```json
{
  "agent_servers": {
    "claude-code-acp": {
      "agent_id": "claude-code-acp",
      "system_prompt": {
        "source": "embedded",
        "template": "",
        "enabled": true
      },
      "user_prompt": {
        "template": "{user_prompt}",
        "enabled": false
      }
    }
  },
  "context_servers": {
    "context7": { ... },
    "fetch": { ... }
  }
}
```

---

## 4. 详细实现计划

### 4.1 阶段一：新增 ChatAgentConfig 结构体

#### 4.1.1 创建新文件

**文件**: `crates/shared_types/src/chat_agent_config.rs`

内容见上文 **2.1 节**。

#### 4.1.2 导出新类型

**文件**: `crates/shared_types/src/lib.rs`

```rust
mod chat_agent_config;
pub use chat_agent_config::*;
```

---

### 4.2 阶段二：修改 HTTP/gRPC 请求结构

#### 4.2.1 修改 rcoder ChatRequest (HTTP)

**文件**: `crates/rcoder/src/handler/chat_handler.rs`

```rust
use shared_types::ChatAgentConfig;

#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct ChatRequest {
    // ... 现有字段 ...

    /// 可选的系统提示词，覆盖默认配置
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "你是一个专业的 Rust 开发者")]
    pub system_prompt: Option<String>,

    /// 可选的用户提示词模板，支持 {user_prompt} 变量替换
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "请用 Rust 完成：{user_prompt}")]
    pub user_prompt: Option<String>,

    /// 可选的 Agent 运行时配置（MCP 服务器等）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<ChatAgentConfig>,
}
```

#### 4.2.2 修改 gRPC ChatRequest

**文件**: `crates/shared_types/proto/agent.proto`

```protobuf
message ChatRequest {
  string project_id = 1;
  string session_id = 2;
  string prompt = 3;
  optional ModelProviderConfig model_config = 4;
  repeated Attachment attachments = 5;
  optional string request_id = 6;
  repeated string data_source_attachments = 7;

  // 新增字段
  optional string system_prompt = 8;              // 系统提示词
  optional string user_prompt = 9;                // 用户提示词模板
  optional ChatAgentConfig agent_config = 10;     // Agent 运行时配置
}

// Agent 运行时配置
message ChatAgentConfig {
  optional ChatAgentServerConfig agent_server = 1;              // 单个 Agent 配置
  map<string, ChatContextServerConfig> context_servers = 2;     // 多个 MCP 配置
}

// 单个 Agent 服务器配置
message ChatAgentServerConfig {
  optional string agent_id = 1;           // Agent 标识符
  optional string command = 2;            // 执行命令
  repeated string args = 3;               // 命令参数
  map<string, string> env = 4;            // 环境变量
  map<string, string> metadata = 5;       // 元数据
}

// MCP 服务器配置
message ChatContextServerConfig {
  string source = 1;                      // "custom" 或 "local"
  bool enabled = 2;
  optional string command = 3;
  repeated string args = 4;
  map<string, string> env = 5;
}
```

---

### 4.3 阶段三：修改 shared_types 数据结构

#### 4.3.1 修改 ChatPrompt

**文件**: `crates/shared_types/src/lib.rs`

```rust
use crate::ChatAgentConfig;

#[derive(Debug, Clone, Builder)]
pub struct ChatPrompt {
    // ... 现有字段 ...

    /// 可选的系统提示词覆盖
    #[builder(default)]
    pub system_prompt_override: Option<String>,

    /// 可选的用户提示词模板覆盖
    #[builder(default)]
    pub user_prompt_template_override: Option<String>,

    /// 可选的 Agent 运行时配置覆盖
    #[builder(default)]
    pub agent_config_override: Option<ChatAgentConfig>,
}
```

---

### 4.4 阶段四：修改 agent_abstraction 层

#### 4.4.1 修改 PromptMessage

**文件**: `crates/agent_abstraction/src/traits/agent.rs`

```rust
use shared_types::ChatAgentConfig;

#[derive(Debug, Clone)]
pub struct PromptMessage {
    // ... 现有字段 ...

    /// 系统提示词覆盖
    pub system_prompt_override: Option<String>,

    /// 用户提示词模板覆盖
    pub user_prompt_template_override: Option<String>,

    /// Agent 运行时配置覆盖（MCP 服务器等）
    pub agent_config_override: Option<ChatAgentConfig>,
}
```

#### 4.4.2 修改 From<ChatPrompt> 实现

```rust
impl From<shared_types::ChatPrompt> for PromptMessage {
    fn from(chat_prompt: shared_types::ChatPrompt) -> Self {
        Self {
            // ... 现有字段映射 ...
            system_prompt_override: chat_prompt.system_prompt_override,
            user_prompt_template_override: chat_prompt.user_prompt_template_override,
            agent_config_override: chat_prompt.agent_config_override,
        }
    }
}
```

---

### 4.5 阶段五：新增配置组装工具

#### 4.5.1 新增 PromptConfigAssembler

**文件**: `crates/agent_config/src/config/prompt_assembler.rs` (新文件)

```rust
//! 提示词配置组装工具
//!
//! 将用户入参组装为内部使用的配置结构。
//! 简化设计：直接使用入参，无优先级冲突。

use shared_types::{ChatAgentConfig, ChatAgentServerConfig};
use super::servers_config::AgentServersConfig;
use crate::{AgentConfig, ContextServerConfig};
use std::collections::HashMap;

/// 提示词配置组装器
///
/// 职责：
/// 1. 组装系统提示词（入参 > 默认配置）
/// 2. 应用用户提示词模板
/// 3. 组装 Agent 服务器配置
/// 4. 合并 MCP 服务器配置
pub struct PromptConfigAssembler {
    /// 系统提示词入参
    system_prompt: Option<String>,
    /// 用户提示词模板入参
    user_prompt_template: Option<String>,
    /// Agent 运行时配置入参
    agent_config: Option<ChatAgentConfig>,
    /// 默认配置
    default_config: AgentServersConfig,
}

impl PromptConfigAssembler {
    pub fn new(default_config: AgentServersConfig) -> Self {
        Self {
            system_prompt: None,
            user_prompt_template: None,
            agent_config: None,
            default_config,
        }
    }

    pub fn with_system_prompt(mut self, system_prompt: Option<String>) -> Self {
        self.system_prompt = system_prompt;
        self
    }

    pub fn with_user_prompt_template(mut self, template: Option<String>) -> Self {
        self.user_prompt_template = template;
        self
    }

    pub fn with_agent_config(mut self, config: Option<ChatAgentConfig>) -> Self {
        self.agent_config = config;
        self
    }

    /// 获取最终的系统提示词
    ///
    /// 逻辑：入参有值则使用入参，否则使用默认配置
    pub fn get_system_prompt(&self, agent_id: &str) -> String {
        // 入参有值且非空，直接使用
        if let Some(ref sp) = self.system_prompt {
            if !sp.is_empty() {
                return sp.clone();
            }
        }

        // 使用默认配置
        self.default_config.get_system_prompt(agent_id)
    }

    /// 应用用户提示词模板
    ///
    /// 逻辑：
    /// 1. 如果有模板入参，使用模板替换 {user_prompt}
    /// 2. 如果没有模板入参，检查默认配置
    /// 3. 都没有，直接返回原始输入
    pub fn apply_user_prompt(&self, agent_id: &str, user_input: &str) -> String {
        // 入参有模板且非空，使用入参模板
        if let Some(ref template) = self.user_prompt_template {
            if !template.is_empty() {
                return template.replace("{user_prompt}", user_input);
            }
        }

        // 检查默认配置中的 user_prompt 模板
        if let Some(agent) = self.default_config.get_agent(agent_id) {
            if let Some(ref prompt_config) = agent.user_prompt {
                if prompt_config.enabled {
                    return prompt_config.apply(user_input);
                }
            }
        }

        // 无模板，直接返回原始输入
        user_input.to_string()
    }

    /// 获取最终的 Agent 服务器配置
    ///
    /// 逻辑：
    /// 1. 如果入参有 agent_server，与默认配置合并（入参字段覆盖默认值）
    /// 2. 如果入参没有 agent_server，使用默认配置
    pub fn get_agent_server_config(&self, default_agent_id: &str) -> AgentConfig {
        // 获取默认的 Agent 配置
        let default_agent = self.default_config
            .get_agent(default_agent_id)
            .cloned()
            .unwrap_or_default();

        // 如果入参有 agent_server 配置，合并覆盖
        if let Some(ref config) = self.agent_config {
            if let Some(ref agent_server) = config.agent_server {
                return self.merge_agent_config(&default_agent, agent_server);
            }
        }

        // 使用默认配置
        default_agent
    }

    /// 合并 Agent 配置（入参覆盖默认值）
    fn merge_agent_config(
        &self,
        default: &AgentConfig,
        override_config: &ChatAgentServerConfig,
    ) -> AgentConfig {
        let mut merged = default.clone();

        // agent_id: 入参有值则覆盖
        if let Some(ref agent_id) = override_config.agent_id {
            merged.agent_id = agent_id.clone();
        }

        // command: 入参有值则覆盖
        if let Some(ref command) = override_config.command {
            merged.command = command.clone();
        }

        // args: 入参有值则覆盖（替换而非追加）
        if let Some(ref args) = override_config.args {
            merged.args = args.clone();
        }

        // env: 入参有值则合并（入参优先）
        if let Some(ref env) = override_config.env {
            for (key, value) in env {
                merged.env.insert(key.clone(), value.clone());
            }
        }

        // metadata: 入参有值则合并
        if let Some(ref metadata) = override_config.metadata {
            for (key, value) in metadata {
                merged.metadata.insert(key.clone(), value.clone());
            }
        }

        merged
    }

    /// 获取最终的 MCP 服务器配置
    ///
    /// 逻辑：入参有配置则使用入参，否则使用默认配置
    pub fn get_context_servers(&self) -> HashMap<String, ContextServerConfig> {
        // 入参有 MCP 配置，使用入参
        if let Some(ref config) = self.agent_config {
            if config.has_context_servers() {
                return config.context_servers.iter()
                    .map(|(name, chat_config)| {
                        let ctx_config = ContextServerConfig {
                            source: chat_config.source.clone(),
                            enabled: chat_config.enabled,
                            command: chat_config.command.clone(),
                            args: chat_config.args.clone(),
                            env: chat_config.env.clone(),
                        };
                        (name.clone(), ctx_config)
                    })
                    .collect();
            }
        }

        // 使用默认配置
        self.default_config.context_servers.clone()
    }

    /// 获取使用的 Agent ID
    ///
    /// 逻辑：入参有指定则使用入参，否则使用默认
    pub fn get_agent_id(&self, default_agent_id: &str) -> String {
        if let Some(ref config) = self.agent_config {
            if let Some(ref agent_server) = config.agent_server {
                return agent_server.get_agent_id().to_string();
            }
        }
        default_agent_id.to_string()
    }
}
```

#### 4.5.2 导出新模块

**文件**: `crates/agent_config/src/config/mod.rs`

```rust
pub mod prompt_assembler;
pub use prompt_assembler::PromptConfigAssembler;
```

**文件**: `crates/agent_config/src/lib.rs`

```rust
pub use config::prompt_assembler::PromptConfigAssembler;
```

---

### 4.6 阶段六：修改 agent_runner 业务逻辑

#### 4.6.1 修改 gRPC AgentServiceImpl::chat()

**文件**: `crates/agent_runner/src/grpc/agent_service_impl.rs`

```rust
use shared_types::ChatAgentConfig;

async fn chat(&self, request: Request<GrpcChatRequest>) -> Result<Response<GrpcChatResponse>, Status> {
    let req = request.into_inner();

    // ... 现有验证逻辑 ...

    // 转换 gRPC ChatAgentConfig -> shared_types ChatAgentConfig
    let agent_config_override: Option<ChatAgentConfig> = req.agent_config.map(ChatAgentConfig::from);

    // 构建 ChatPrompt（包含覆盖配置）
    let chat_prompt = ChatPromptBuilder::default()
        .project_id(project_id.clone())
        .project_path(project_dir)
        .session_id(session_id.clone())
        .prompt(req.prompt)
        .system_prompt_override(req.system_prompt)           // 新增
        .user_prompt_template_override(req.user_prompt)      // 新增
        .agent_config_override(agent_config_override)        // 新增
        // ... 其他字段 ...
        .build()
        .map_err(|e| Status::internal(format!("构建 ChatPrompt 失败: {}", e)))?;

    // ... 后续处理 ...
}
```

#### 4.6.2 修改 AcpAgentWorker

**文件**: `crates/agent_abstraction/src/session/acp_worker.rs`

```rust
use agent_config::PromptConfigAssembler;

impl AgentWorker for AcpAgentWorker {
    async fn process_request(&self, request: WorkerRequest) -> Result<WorkerResponse> {
        // 加载默认配置
        let default_config = AgentServersConfig::load_or_default().await;

        // 创建配置组装器
        let assembler = PromptConfigAssembler::new(default_config)
            .with_system_prompt(request.prompt_message.system_prompt_override.clone())
            .with_user_prompt_template(request.prompt_message.user_prompt_template_override.clone())
            .with_agent_config(request.prompt_message.agent_config_override.clone());

        // 获取最终的系统提示词
        let system_prompt = assembler.get_system_prompt("claude-code-acp");

        // 构建 AgentStartConfig
        let start_config = AgentStartConfig::new()
            .with_system_prompt(system_prompt);

        // 获取 MCP 服务器配置
        let context_servers = assembler.get_context_servers();
        // TODO: 将 context_servers 转换为 Vec<McpServer> 传递给 start_config

        // 应用用户提示词模板
        let final_user_prompt = assembler.apply_user_prompt(
            "claude-code-acp",
            &request.prompt_message.content,
        );

        // 更新 prompt_message 的 content 为处理后的用户提示词
        let mut prompt_message = request.prompt_message.clone();
        prompt_message.content = final_user_prompt;

        // ... 创建会话 / 发送 prompt ...
    }
}
```

---

### 4.7 阶段七：修改 ClaudeCodeLauncher

**文件**: `crates/agent_abstraction/src/compat/claude_code_launcher.rs`

```rust
use shared_types::ChatAgentConfig;

pub async fn load_agent_config_with_override(
    model_provider: Option<&ModelProviderConfig>,
    agent_config_override: Option<&ChatAgentConfig>,
) -> Result<AgentLaunchConfig> {
    // 加载默认配置
    let default_config = AgentServersConfig::load_or_default().await;

    // 合并 context_servers
    let context_servers = if let Some(override_config) = agent_config_override {
        if override_config.has_context_servers() {
            // 使用覆盖的 MCP 配置
            override_config.context_servers.iter()
                .map(|(name, config)| {
                    (name.clone(), ContextServerConfig {
                        source: config.source.clone(),
                        enabled: config.enabled,
                        command: config.command.clone(),
                        args: config.args.clone(),
                        env: config.env.clone(),
                    })
                })
                .collect()
        } else {
            default_config.context_servers.clone()
        }
    } else {
        default_config.context_servers.clone()
    };

    // ... 后续使用 context_servers 构建 AgentLaunchConfig ...
}
```

---

## 5. 测试计划

### 5.1 单元测试

1. **ChatAgentConfig 结构体测试**
   - 测试 JSON 序列化/反序列化
   - 测试默认值
   - 测试 `has_agent_server()` 方法
   - 测试 `has_context_servers()` 方法
   - 测试 `get_enabled_context_servers()` 方法

2. **ChatAgentServerConfig 结构体测试**
   - 测试 JSON 序列化/反序列化
   - 测试 `get_agent_id()` 默认值
   - 测试部分字段覆盖

3. **PromptConfigAssembler 测试**
   - 测试入参有值时直接使用入参
   - 测试入参为空时使用默认配置
   - 测试用户提示词模板 `{user_prompt}` 替换
   - 测试 Agent 服务器配置合并（`get_agent_server_config()`）
   - 测试 Agent 配置字段覆盖（command, args, env）
   - 测试 MCP 服务器配置合并

### 5.2 集成测试

```bash
# 测试只传 system_prompt
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{"prompt": "hello", "system_prompt": "你是 Rust 专家"}'

# 测试只传 user_prompt
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{"prompt": "hello", "user_prompt": "请用中文回答：{user_prompt}"}'

# 测试只传 agent_config（仅 agent_server）
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "hello",
    "agent_config": {
      "agent_server": {
        "env": {
          "ANTHROPIC_MODEL": "claude-sonnet-4-20250514",
          "RUST_LOG": "debug"
        }
      }
    }
  }'

# 测试只传 agent_config（仅 MCP 服务器）
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "hello",
    "agent_config": {
      "context_servers": {
        "my-mcp": {
          "enabled": true,
          "command": "bunx",
          "args": ["-y", "my-mcp-server"]
        }
      }
    }
  }'

# 测试 agent_server + context_servers 组合
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "hello",
    "agent_config": {
      "agent_server": {
        "agent_id": "claude-code-acp",
        "env": {"RUST_LOG": "info"}
      },
      "context_servers": {
        "context7": {"enabled": true, "command": "bunx", "args": ["-y", "@upstash/context7-mcp"]}
      }
    }
  }'

# 测试完整组合（所有入参）
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "帮我写一个 Hello World",
    "system_prompt": "你是 Rust 专家",
    "user_prompt": "请用 Rust 完成：{user_prompt}",
    "agent_config": {
      "agent_server": {
        "env": {"ANTHROPIC_MODEL": "claude-sonnet-4-20250514"}
      },
      "context_servers": {
        "context7": {"enabled": true, "command": "bunx", "args": ["-y", "@upstash/context7-mcp"]}
      }
    }
  }'
```

### 5.3 端到端测试
- 验证 Agent 实际使用了自定义的系统提示词
- 验证用户提示词模板替换正确
- 验证自定义 agent_server 的 env 变量生效
- 验证自定义 MCP 服务器配置生效
- 验证 agent_server 配置的 command/args 覆盖生效

---

## 6. 修改文件清单

| 文件 | 修改类型 | 说明 |
|------|----------|------|
| `crates/shared_types/src/chat_agent_config.rs` | **新增** | ChatAgentConfig 结构体（简化版） |
| `crates/shared_types/src/lib.rs` | 修改 | 导出 ChatAgentConfig |
| `crates/shared_types/proto/agent.proto` | 修改 | 添加 gRPC 消息定义 |
| `crates/shared_types/src/grpc/agent.rs` | 自动生成 | proto 编译 |
| `crates/rcoder/src/handler/chat_handler.rs` | 修改 | HTTP ChatRequest 添加新字段 |
| `crates/rcoder/src/grpc/chat_client.rs` | 修改 | 传递新字段到 gRPC |
| `crates/agent_runner/src/grpc/agent_service_impl.rs` | 修改 | 处理新字段 |
| `crates/agent_config/src/config/prompt_assembler.rs` | **新增** | 配置组装逻辑 |
| `crates/agent_config/src/config/mod.rs` | 修改 | 导出新模块 |
| `crates/agent_config/src/lib.rs` | 修改 | 导出 PromptConfigAssembler |
| `crates/agent_abstraction/src/traits/agent.rs` | 修改 | PromptMessage 添加新字段 |
| `crates/agent_abstraction/src/session/acp_worker.rs` | 修改 | 使用配置组装器 |
| `crates/agent_abstraction/src/compat/claude_code_launcher.rs` | 修改 | 支持配置覆盖 |

---

## 7. 风险评估

### 7.1 兼容性风险
- **低风险**: 所有新字段都是可选的，不影响现有调用
- **缓解措施**: 添加版本检查，确保旧客户端可以正常工作

### 7.2 性能风险
- **低风险**: 配置组装逻辑简单，不涉及 I/O
- **缓解措施**: 配置组装器使用惰性求值

### 7.3 安全风险
- **中风险**: `agent_config` 允许用户传入自定义 MCP 服务器配置
- **缓解措施**:
  - 使用强类型结构体验证配置格式
  - 记录所有配置变更的日志
  - 考虑添加白名单机制（可选）

---

## 8. 实现顺序建议

1. **第一批** (基础结构)
   - 创建 `ChatAgentConfig` 结构体 (shared_types)
   - 修改 proto 文件，添加 gRPC 消息定义
   - 修改 ChatRequest (HTTP/gRPC)
   - 修改 ChatPrompt 和 PromptMessage

2. **第二批** (核心逻辑)
   - 实现 PromptConfigAssembler
   - 修改 AgentServiceImpl
   - 修改 AcpAgentWorker

3. **第三批** (启动器)
   - 修改 ClaudeCodeLauncher
   - 添加 gRPC 类型转换

4. **第四批** (测试和文档)
   - 编写单元测试
   - 编写集成测试
   - 更新 API 文档 (Swagger/OpenAPI)

---

## 9. 附录：类型对照表

### 9.1 HTTP/gRPC 层

| 层级 | 类型名 | 用途 |
|------|--------|------|
| HTTP | `ChatRequest.system_prompt: Option<String>` | 接收系统提示词 |
| HTTP | `ChatRequest.user_prompt: Option<String>` | 接收用户提示词模板 |
| HTTP | `ChatRequest.agent_config: Option<ChatAgentConfig>` | 接收 Agent 运行时配置 |
| gRPC | `ChatRequest.system_prompt`, `user_prompt`, `agent_config` | 跨服务传输 |

### 9.2 shared_types 层（外部入参结构）

| 类型名 | 字段 | 用途 |
|--------|------|------|
| `ChatAgentConfig` | `agent_server`, `context_servers` | 外部入参的 Agent 配置（简化版） |
| `ChatAgentServerConfig` | `agent_id`, `command`, `args`, `env`, `metadata` | 单个 Agent 服务器配置 |
| `ChatContextServerConfig` | `source`, `enabled`, `command`, `args`, `env` | 单个 MCP 服务器配置 |

### 9.3 agent_config 层（内部配置结构）

| 类型名 | 字段 | 用途 |
|--------|------|------|
| `AgentServersConfig` | `agent_servers`, `context_servers` | 内部完整配置（支持多 Agent） |
| `AgentConfig` | `agent_id`, `command`, `args`, `env`, `system_prompt`, `user_prompt`, ... | 内部 Agent 配置（包含提示词模板） |
| `ContextServerConfig` | `source`, `enabled`, `command`, `args`, `env` | 内部 MCP 配置 |

### 9.4 业务逻辑层

| 层级 | 类型名 | 用途 |
|------|--------|------|
| agent_config | `PromptConfigAssembler` | 配置组装逻辑（外部 → 内部） |
| agent_abstraction | `PromptMessage.*_override` | Agent 层配置传递 |
| agent_abstraction | `AgentStartConfig` | Agent 启动时的最终配置 |

### 9.5 外部入参 vs 内部配置 映射关系

```
外部入参 (ChatAgentConfig)              内部配置 (AgentServersConfig)
─────────────────────────────────────────────────────────────────────
ChatAgentServerConfig (单个)       →    AgentConfig (HashMap 中的一个)
  - agent_id                       →      - agent_id
  - command                        →      - command
  - args                           →      - args
  - env                            →      - env
  - metadata                       →      - metadata
  (无 system_prompt)               →      - system_prompt (保留默认)
  (无 user_prompt)                 →      - user_prompt (保留默认)

ChatContextServerConfig (多个)     →    ContextServerConfig (多个)
  - source                         →      - source
  - enabled                        →      - enabled
  - command                        →      - command
  - args                           →      - args
  - env                            →      - env
```

---

## 10. 设计对比：简化方案 vs 原方案

| 方面 | 原方案 | 简化方案（当前） |
|------|--------|------------------|
| `agent_config` 内容 | 包含 `system_prompt`, `user_prompt`, `context_servers` | 包含 `agent_server` (单个) + `context_servers` (多个) |
| 提示词配置 | 在 `agent_config` 中 | 独立入参 (`system_prompt`, `user_prompt`) |
| 优先级冲突 | 存在（入参 vs agent_config 中的提示词） | 无（职责分离） |
| Agent 配置 | 无法覆盖 | 支持单个 Agent 的 command/args/env 覆盖 |
| MCP 配置 | 可配置多个 | 可配置多个（保持一致） |
| 合并逻辑 | 复杂的三级优先级 | 简单的二级（入参 > 默认） |
| 用户理解成本 | 高（需要理解优先级规则） | 低（职责分离，直观） |
| 代码复杂度 | ConfigMerger 需要处理多种情况 | PromptConfigAssembler 逻辑简单 |
