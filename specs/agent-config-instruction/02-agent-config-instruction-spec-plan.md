# Agent 配置入参功能 - 详细实现计划

> 基于 `01-agent-config-instruction-spec.md` 设计文档生成

## 1. 实现概述

### 1.1 目标
为 `/chat` 接口增加三个可选配置入参：
- `system_prompt`: 系统提示词
- `user_prompt`: 用户提示词模板（支持 `{user_prompt}` 变量注入）
- `agent_config`: Agent 运行时配置（包含 `agent_server` 和 `context_servers`）

### 1.2 实现原则
- **向后兼容**: 所有新字段可选，不影响现有调用
- **职责分离**: 提示词由独立入参控制，`agent_config` 只负责运行时配置
- **类型安全**: 使用强类型结构体，避免 JSON 字符串解析

### 1.3 实现分批
| 批次 | 内容 | 预计文件数 |
|------|------|-----------|
| 第一批 | 基础结构（类型定义、proto、请求结构） | 5 |
| 第二批 | 核心逻辑（配置组装器、gRPC 服务） | 4 |
| 第三批 | 启动器集成（ClaudeCodeLauncher） | 2 |
| 第四批 | 测试和文档 | 3+ |

---

## 2. 第一批：基础结构

### 2.1 任务清单

| 序号 | 任务 | 文件 | 类型 |
|------|------|------|------|
| 1.1 | 创建 ChatAgentConfig 结构体 | `crates/shared_types/src/chat_agent_config.rs` | 新增 |
| 1.2 | 导出新类型 | `crates/shared_types/src/lib.rs` | 修改 |
| 1.3 | 修改 gRPC proto 定义 | `crates/shared_types/proto/agent.proto` | 修改 |
| 1.4 | 修改 HTTP ChatRequest | `crates/rcoder/src/handler/chat_handler.rs` | 修改 |
| 1.5 | 修改 gRPC 客户端传参 | `crates/rcoder/src/grpc/chat_client.rs` | 修改 |

---

### 2.1.1 创建 ChatAgentConfig 结构体

**文件**: `crates/shared_types/src/chat_agent_config.rs`

**步骤**:
1. 创建新文件
2. 定义三个结构体：
   - `ChatAgentConfig` - 顶层配置
   - `ChatAgentServerConfig` - Agent 服务器配置
   - `ChatContextServerConfig` - MCP 服务器配置
3. 实现 `Default` trait
4. 实现辅助方法：`has_agent_server()`, `has_context_servers()`, `get_enabled_context_servers()`, `get_agent_id()`

**代码模板**:
```rust
// 见设计文档 2.2 节完整代码
```

**验证点**:
- [ ] 编译通过
- [ ] JSON 序列化/反序列化测试通过
- [ ] 默认值正确

---

### 2.1.2 导出新类型

**文件**: `crates/shared_types/src/lib.rs`

**修改内容**:
```rust
// 新增
mod chat_agent_config;
pub use chat_agent_config::{ChatAgentConfig, ChatAgentServerConfig, ChatContextServerConfig};
```

**验证点**:
- [ ] `cargo build -p shared_types` 编译通过
- [ ] 其他 crate 可以正常引用新类型

---

### 2.1.3 修改 gRPC proto 定义

**文件**: `crates/shared_types/proto/agent.proto`

**修改内容**:

1. 在 `ChatRequest` 消息中添加新字段：
```protobuf
message ChatRequest {
  // ... 现有字段 (1-7) ...

  // 新增字段
  optional string system_prompt = 8;              // 系统提示词
  optional string user_prompt = 9;                // 用户提示词模板
  optional ChatAgentConfig agent_config = 10;     // Agent 运行时配置
}
```

2. 添加新消息定义：
```protobuf
// Agent 运行时配置
message ChatAgentConfig {
  optional ChatAgentServerConfig agent_server = 1;
  map<string, ChatContextServerConfig> context_servers = 2;
}

// 单个 Agent 服务器配置
message ChatAgentServerConfig {
  optional string agent_id = 1;
  optional string command = 2;
  repeated string args = 3;
  map<string, string> env = 4;
  map<string, string> metadata = 5;
}

// MCP 服务器配置
message ChatContextServerConfig {
  string source = 1;
  bool enabled = 2;
  optional string command = 3;
  repeated string args = 4;
  map<string, string> env = 5;
}
```

**验证点**:
- [ ] `cargo build -p shared_types` 编译通过（proto 自动生成）
- [ ] 生成的 Rust 代码位于 `src/grpc/agent.rs`

---

### 2.1.4 修改 HTTP ChatRequest

**文件**: `crates/rcoder/src/handler/chat_handler.rs`

**修改内容**:

1. 添加 import：
```rust
use shared_types::ChatAgentConfig;
```

2. 在 `ChatRequest` 结构体中添加新字段：
```rust
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

    /// 可选的 Agent 运行时配置（Agent 服务器 + MCP 服务器）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<ChatAgentConfig>,
}
```

**验证点**:
- [ ] `cargo build -p rcoder` 编译通过
- [ ] OpenAPI 文档正确生成新字段

---

### 2.1.5 修改 gRPC 客户端传参

**文件**: `crates/rcoder/src/grpc/chat_client.rs`

**修改内容**:

1. 在构建 gRPC `ChatRequest` 时传递新字段：
```rust
// 找到构建 GrpcChatRequest 的位置，添加新字段
let grpc_request = GrpcChatRequest {
    // ... 现有字段 ...
    system_prompt: http_request.system_prompt.clone(),
    user_prompt: http_request.user_prompt.clone(),
    agent_config: http_request.agent_config.clone().map(|c| c.into()),
};
```

2. 实现 `From<ChatAgentConfig>` 转换（如需要）：
```rust
impl From<shared_types::ChatAgentConfig> for proto::ChatAgentConfig {
    fn from(config: shared_types::ChatAgentConfig) -> Self {
        Self {
            agent_server: config.agent_server.map(|s| s.into()),
            context_servers: config.context_servers
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}
```

**验证点**:
- [ ] `cargo build -p rcoder` 编译通过
- [ ] gRPC 请求正确包含新字段

---

## 3. 第二批：核心逻辑

### 3.1 任务清单

| 序号 | 任务 | 文件 | 类型 |
|------|------|------|------|
| 2.1 | 创建 PromptConfigAssembler | `crates/agent_config/src/config/prompt_assembler.rs` | 新增 |
| 2.2 | 导出新模块 | `crates/agent_config/src/config/mod.rs` | 修改 |
| 2.3 | 修改 ChatPrompt 结构体 | `crates/shared_types/src/lib.rs` | 修改 |
| 2.4 | 修改 PromptMessage 结构体 | `crates/agent_abstraction/src/traits/agent.rs` | 修改 |

---

### 3.1.1 创建 PromptConfigAssembler

**文件**: `crates/agent_config/src/config/prompt_assembler.rs`

**步骤**:
1. 创建新文件
2. 实现 `PromptConfigAssembler` 结构体
3. 实现核心方法：
   - `new()` - 构造函数
   - `with_system_prompt()` - Builder 方法
   - `with_user_prompt_template()` - Builder 方法
   - `with_agent_config()` - Builder 方法
   - `get_system_prompt()` - 获取最终系统提示词
   - `apply_user_prompt()` - 应用用户提示词模板
   - `get_agent_server_config()` - 获取最终 Agent 配置
   - `get_context_servers()` - 获取最终 MCP 配置
   - `get_agent_id()` - 获取使用的 Agent ID

**代码模板**:
```rust
// 见设计文档 4.5.1 节完整代码
```

**验证点**:
- [ ] 编译通过
- [ ] 单元测试：入参优先级正确
- [ ] 单元测试：模板替换正确
- [ ] 单元测试：配置合并正确

---

### 3.1.2 导出新模块

**文件**: `crates/agent_config/src/config/mod.rs`

**修改内容**:
```rust
pub mod prompt_assembler;
pub use prompt_assembler::PromptConfigAssembler;
```

**文件**: `crates/agent_config/src/lib.rs`

**修改内容**:
```rust
pub use config::prompt_assembler::PromptConfigAssembler;
```

**验证点**:
- [ ] `cargo build -p agent_config` 编译通过
- [ ] 其他 crate 可以正常引用 `PromptConfigAssembler`

---

### 3.1.3 修改 ChatPrompt 结构体

**文件**: `crates/shared_types/src/lib.rs` (或 `chat_prompt.rs`)

**修改内容**:
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

**验证点**:
- [ ] `cargo build -p shared_types` 编译通过
- [ ] Builder 模式正常工作

---

### 3.1.4 修改 PromptMessage 结构体

**文件**: `crates/agent_abstraction/src/traits/agent.rs`

**修改内容**:

1. 添加 import：
```rust
use shared_types::ChatAgentConfig;
```

2. 在 `PromptMessage` 结构体中添加新字段：
```rust
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

3. 修改 `From<ChatPrompt>` 实现：
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

**验证点**:
- [ ] `cargo build -p agent_abstraction` 编译通过
- [ ] `From` 转换正确

---

## 4. 第三批：服务层集成

### 4.1 任务清单

| 序号 | 任务 | 文件 | 类型 |
|------|------|------|------|
| 3.1 | 修改 gRPC AgentServiceImpl | `crates/agent_runner/src/grpc/agent_service_impl.rs` | 修改 |
| 3.2 | 修改 AcpAgentWorker | `crates/agent_abstraction/src/session/acp_worker.rs` | 修改 |
| 3.3 | 修改 ClaudeCodeLauncher | `crates/agent_abstraction/src/compat/claude_code_launcher.rs` | 修改 |

---

### 4.1.1 修改 gRPC AgentServiceImpl

**文件**: `crates/agent_runner/src/grpc/agent_service_impl.rs`

**修改内容**:

在 `chat()` 方法中：

1. 添加 import：
```rust
use shared_types::ChatAgentConfig;
```

2. 解析新字段并传递给 ChatPrompt：
```rust
async fn chat(&self, request: Request<GrpcChatRequest>) -> Result<Response<GrpcChatResponse>, Status> {
    let req = request.into_inner();

    // ... 现有验证逻辑 ...

    // 转换 gRPC ChatAgentConfig -> shared_types ChatAgentConfig
    let agent_config_override: Option<ChatAgentConfig> = req.agent_config.map(|c| c.into());

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

3. 实现 gRPC 类型转换（如果需要）：
```rust
impl From<proto::ChatAgentConfig> for shared_types::ChatAgentConfig {
    fn from(proto: proto::ChatAgentConfig) -> Self {
        Self {
            agent_server: proto.agent_server.map(|s| s.into()),
            context_servers: proto.context_servers
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}

impl From<proto::ChatAgentServerConfig> for shared_types::ChatAgentServerConfig {
    fn from(proto: proto::ChatAgentServerConfig) -> Self {
        Self {
            agent_id: proto.agent_id,
            command: proto.command,
            args: if proto.args.is_empty() { None } else { Some(proto.args) },
            env: if proto.env.is_empty() { None } else { Some(proto.env) },
            metadata: if proto.metadata.is_empty() { None } else { Some(proto.metadata) },
        }
    }
}

impl From<proto::ChatContextServerConfig> for shared_types::ChatContextServerConfig {
    fn from(proto: proto::ChatContextServerConfig) -> Self {
        Self {
            source: proto.source,
            enabled: proto.enabled,
            command: proto.command,
            args: if proto.args.is_empty() { None } else { Some(proto.args) },
            env: if proto.env.is_empty() { None } else { Some(proto.env) },
        }
    }
}
```

**验证点**:
- [ ] `cargo build -p agent_runner` 编译通过
- [ ] gRPC 类型转换正确

---

### 4.1.2 修改 AcpAgentWorker

**文件**: `crates/agent_abstraction/src/session/acp_worker.rs`

**修改内容**:

1. 添加 import：
```rust
use agent_config::{AgentServersConfig, PromptConfigAssembler};
```

2. 在处理请求时使用配置组装器：
```rust
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

        // 获取最终的 Agent 配置
        let agent_config = assembler.get_agent_server_config("claude-code-acp");

        // 获取 MCP 服务器配置
        let context_servers = assembler.get_context_servers();

        // 应用用户提示词模板
        let final_user_prompt = assembler.apply_user_prompt(
            "claude-code-acp",
            &request.prompt_message.content,
        );

        // 更新 prompt_message 的 content 为处理后的用户提示词
        let mut prompt_message = request.prompt_message.clone();
        prompt_message.content = final_user_prompt;

        // 构建 AgentStartConfig
        let start_config = AgentStartConfig::new()
            .with_system_prompt(system_prompt)
            .with_mcp_servers(context_servers);  // 如果有此方法

        // ... 创建会话 / 发送 prompt ...
    }
}
```

**验证点**:
- [ ] `cargo build -p agent_abstraction` 编译通过
- [ ] 配置组装逻辑正确

---

### 4.1.3 修改 ClaudeCodeLauncher

**文件**: `crates/agent_abstraction/src/compat/claude_code_launcher.rs`

**修改内容**:

1. 添加 import：
```rust
use shared_types::ChatAgentConfig;
use agent_config::PromptConfigAssembler;
```

2. 修改或新增配置加载方法：
```rust
/// 加载 Agent 配置，支持覆盖
pub async fn load_agent_config_with_override(
    model_provider: Option<&ModelProviderConfig>,
    agent_config_override: Option<&ChatAgentConfig>,
) -> Result<AgentLaunchConfig> {
    // 加载默认配置
    let default_config = AgentServersConfig::load_or_default().await;

    // 创建配置组装器
    let assembler = PromptConfigAssembler::new(default_config.clone())
        .with_agent_config(agent_config_override.cloned());

    // 获取最终的 Agent 配置
    let agent_config = assembler.get_agent_server_config("claude-code-acp");

    // 获取 MCP 服务器配置
    let context_servers = assembler.get_context_servers();

    // 构建启动配置
    // ... 使用 agent_config 和 context_servers 构建 AgentLaunchConfig ...
}
```

3. 在 `launch()` 方法中使用新配置：
```rust
pub async fn launch(
    &self,
    project_id: String,
    project_path: PathBuf,
    session_id_hint: Option<String>,
    model_provider: Option<ModelProviderConfig>,
    start_config: AgentStartConfig,  // 已包含系统提示词和 MCP 配置
    client: C,
) -> Result<ConnectionInfo> {
    // 从 start_config 获取配置
    let system_prompt = start_config.system_prompt.clone();
    let mcp_servers = start_config.mcp_servers.clone();

    // ... 使用配置启动 Agent ...
}
```

**验证点**:
- [ ] `cargo build -p agent_abstraction` 编译通过
- [ ] Agent 启动时正确使用覆盖配置

---

## 5. 第四批：测试和文档

### 5.1 任务清单

| 序号 | 任务 | 文件 | 类型 |
|------|------|------|------|
| 4.1 | ChatAgentConfig 单元测试 | `crates/shared_types/src/chat_agent_config.rs` | 修改 |
| 4.2 | PromptConfigAssembler 单元测试 | `crates/agent_config/src/config/prompt_assembler.rs` | 修改 |
| 4.3 | 集成测试 | `tests/integration/chat_config_test.rs` | 新增 |
| 4.4 | 更新 OpenAPI 文档 | 自动生成 | - |

---

### 5.1.1 ChatAgentConfig 单元测试

**文件**: `crates/shared_types/src/chat_agent_config.rs`

**测试用例**:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_agent_config_default() {
        let config = ChatAgentConfig::default();
        assert!(config.agent_server.is_none());
        assert!(config.context_servers.is_empty());
        assert!(!config.has_agent_server());
        assert!(!config.has_context_servers());
    }

    #[test]
    fn test_chat_agent_config_json_serialize() {
        let config = ChatAgentConfig {
            agent_server: Some(ChatAgentServerConfig {
                agent_id: Some("test-agent".to_string()),
                command: Some("test-cmd".to_string()),
                ..Default::default()
            }),
            context_servers: HashMap::new(),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("test-agent"));
    }

    #[test]
    fn test_chat_agent_config_json_deserialize() {
        let json = r#"{
            "agent_server": {
                "agent_id": "claude-code-acp",
                "env": {"RUST_LOG": "debug"}
            },
            "context_servers": {
                "context7": {
                    "source": "custom",
                    "enabled": true,
                    "command": "bunx",
                    "args": ["-y", "@upstash/context7-mcp"]
                }
            }
        }"#;
        let config: ChatAgentConfig = serde_json::from_str(json).unwrap();
        assert!(config.has_agent_server());
        assert!(config.has_context_servers());
        assert_eq!(config.agent_server.unwrap().get_agent_id(), "claude-code-acp");
    }

    #[test]
    fn test_get_agent_id_default() {
        let config = ChatAgentServerConfig::default();
        assert_eq!(config.get_agent_id(), "claude-code-acp");
    }

    #[test]
    fn test_get_enabled_context_servers() {
        let mut context_servers = HashMap::new();
        context_servers.insert("enabled".to_string(), ChatContextServerConfig {
            enabled: true,
            ..Default::default()
        });
        context_servers.insert("disabled".to_string(), ChatContextServerConfig {
            enabled: false,
            ..Default::default()
        });
        let config = ChatAgentConfig {
            agent_server: None,
            context_servers,
        };
        let enabled = config.get_enabled_context_servers();
        assert_eq!(enabled.len(), 1);
        assert!(enabled.contains_key("enabled"));
    }
}
```

---

### 5.1.2 PromptConfigAssembler 单元测试

**文件**: `crates/agent_config/src/config/prompt_assembler.rs`

**测试用例**:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn create_default_config() -> AgentServersConfig {
        // 创建测试用的默认配置
        AgentServersConfig::default()
    }

    #[test]
    fn test_system_prompt_override() {
        let assembler = PromptConfigAssembler::new(create_default_config())
            .with_system_prompt(Some("自定义系统提示词".to_string()));

        let result = assembler.get_system_prompt("claude-code-acp");
        assert_eq!(result, "自定义系统提示词");
    }

    #[test]
    fn test_system_prompt_use_default() {
        let assembler = PromptConfigAssembler::new(create_default_config());

        // 应该使用默认配置
        let result = assembler.get_system_prompt("claude-code-acp");
        // 验证使用了默认配置
    }

    #[test]
    fn test_user_prompt_template() {
        let assembler = PromptConfigAssembler::new(create_default_config())
            .with_user_prompt_template(Some("请用中文回答：{user_prompt}".to_string()));

        let result = assembler.apply_user_prompt("claude-code-acp", "hello world");
        assert_eq!(result, "请用中文回答：hello world");
    }

    #[test]
    fn test_user_prompt_no_template() {
        let assembler = PromptConfigAssembler::new(create_default_config());

        let result = assembler.apply_user_prompt("claude-code-acp", "hello world");
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_agent_server_config_merge() {
        let override_config = ChatAgentConfig {
            agent_server: Some(ChatAgentServerConfig {
                env: Some(HashMap::from([
                    ("RUST_LOG".to_string(), "debug".to_string()),
                ])),
                ..Default::default()
            }),
            context_servers: HashMap::new(),
        };

        let assembler = PromptConfigAssembler::new(create_default_config())
            .with_agent_config(Some(override_config));

        let result = assembler.get_agent_server_config("claude-code-acp");
        // 验证 env 被正确合并
        assert!(result.env.contains_key("RUST_LOG"));
    }

    #[test]
    fn test_context_servers_override() {
        let mut context_servers = HashMap::new();
        context_servers.insert("my-mcp".to_string(), ChatContextServerConfig {
            source: "custom".to_string(),
            enabled: true,
            command: Some("bunx".to_string()),
            args: Some(vec!["-y".to_string(), "my-mcp-server".to_string()]),
            env: None,
        });

        let override_config = ChatAgentConfig {
            agent_server: None,
            context_servers,
        };

        let assembler = PromptConfigAssembler::new(create_default_config())
            .with_agent_config(Some(override_config));

        let result = assembler.get_context_servers();
        assert!(result.contains_key("my-mcp"));
    }
}
```

---

### 5.1.3 集成测试

**文件**: `tests/integration/chat_config_test.rs` (或在现有测试文件中添加)

**测试用例**:
```rust
#[tokio::test]
async fn test_chat_with_system_prompt() {
    // 测试只传 system_prompt
    let response = client
        .post("/chat")
        .json(&json!({
            "prompt": "hello",
            "project_id": "test",
            "system_prompt": "你是 Rust 专家"
        }))
        .send()
        .await
        .unwrap();

    assert!(response.status().is_success());
}

#[tokio::test]
async fn test_chat_with_user_prompt_template() {
    // 测试 user_prompt 模板替换
    let response = client
        .post("/chat")
        .json(&json!({
            "prompt": "hello world",
            "project_id": "test",
            "user_prompt": "请用中文回答：{user_prompt}"
        }))
        .send()
        .await
        .unwrap();

    assert!(response.status().is_success());
}

#[tokio::test]
async fn test_chat_with_agent_config() {
    // 测试完整的 agent_config
    let response = client
        .post("/chat")
        .json(&json!({
            "prompt": "hello",
            "project_id": "test",
            "agent_config": {
                "agent_server": {
                    "env": {"RUST_LOG": "debug"}
                },
                "context_servers": {
                    "context7": {
                        "enabled": true,
                        "command": "bunx",
                        "args": ["-y", "@upstash/context7-mcp"]
                    }
                }
            }
        }))
        .send()
        .await
        .unwrap();

    assert!(response.status().is_success());
}
```

---

## 6. 实现检查清单

### 6.1 第一批完成检查

- [ ] `ChatAgentConfig` 结构体创建完成
- [ ] `ChatAgentServerConfig` 结构体创建完成
- [ ] `ChatContextServerConfig` 结构体创建完成
- [ ] `shared_types` 导出新类型
- [ ] `agent.proto` 添加新消息定义
- [ ] HTTP `ChatRequest` 添加新字段
- [ ] gRPC 客户端传递新字段
- [ ] `cargo build` 全部通过

### 6.2 第二批完成检查

- [ ] `PromptConfigAssembler` 创建完成
- [ ] `agent_config` 导出新模块
- [ ] `ChatPrompt` 添加新字段
- [ ] `PromptMessage` 添加新字段
- [ ] `From` 转换实现完成
- [ ] `cargo build` 全部通过

### 6.3 第三批完成检查

- [ ] `AgentServiceImpl::chat()` 处理新字段
- [ ] gRPC 类型转换实现完成
- [ ] `AcpAgentWorker` 使用配置组装器
- [ ] `ClaudeCodeLauncher` 支持配置覆盖
- [ ] `cargo build` 全部通过

### 6.4 第四批完成检查

- [ ] `ChatAgentConfig` 单元测试通过
- [ ] `PromptConfigAssembler` 单元测试通过
- [ ] 集成测试通过
- [ ] OpenAPI 文档更新
- [ ] `cargo test` 全部通过

---

## 7. 风险和注意事项

### 7.1 向后兼容性
- 所有新字段都是 `Option` 类型，确保旧客户端正常工作
- proto 字段编号从 8 开始，不影响现有字段

### 7.2 类型转换
- 注意 gRPC 生成的类型和 Rust 原生类型的转换
- `repeated` 字段为空时转换为 `None`
- `map` 字段为空时转换为 `None`

### 7.3 配置合并逻辑
- `env` 和 `metadata` 是合并（入参优先）
- `args` 是替换（入参完全覆盖）
- 提示词是覆盖（入参有值则使用入参）

### 7.4 测试覆盖
- 确保测试覆盖所有配置组合
- 特别注意边界情况：空字符串、空 HashMap 等

---

## 8. 预计工作量

| 批次 | 预计工作量 | 依赖 |
|------|-----------|------|
| 第一批 | 2-3 小时 | 无 |
| 第二批 | 2-3 小时 | 第一批 |
| 第三批 | 2-3 小时 | 第二批 |
| 第四批 | 1-2 小时 | 第三批 |

**总计**: 约 7-11 小时
