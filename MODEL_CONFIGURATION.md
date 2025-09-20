# AI 代理模型配置指南

## 概述

本指南详细说明如何配置国内大模型（如智谱 GLM）来接入 Codex 或 Claude Code 代理工具，实现在不同代理工具中使用相同的大模型服务。

## 架构理解

### 核心概念

- **AgentType**: 代表底层使用的工具（`Codex`、`Claude`）
- **Model**: 代表具体的模型名称（`GLM-4.5`、`GPT-4`、`claude-3-5-sonnet`）
- **Provider**: 代表模型提供商配置（API 端点、认证等）

### 关系说明

```
用户请求
    ↓
AgentManager（选择代理工具）
    ↓
┌─────────────────┬─────────────────┐
│   Claude Agent  │   Codex Agent   │
│                 │                 │
│ ┌─────────────┐ │ ┌─────────────┐ │
│ │ GLM via     │ │ │ GLM via     │ │
│ │ Anthropic   │ │ │ OpenAI      │ │
│ │ Interface   │ │ │ Interface   │ │
│ └─────────────┘ │ └─────────────┘ │
│                 │                 │
│ ┌─────────────┐ │ ┌─────────────┐ │
│ │ Standard    │ │ │ Standard    │ │
│ │ Claude      │ │ │ OpenAI      │ │
│ └─────────────┘ │ └─────────────┘ │
└─────────────────┴─────────────────┘
```

GLM-4.5 可以通过两种方式使用：
1. **通过 Codex 代理**: 使用 OpenAI 兼容接口
2. **通过 Claude 代理**: 使用 Anthropic 兼容接口

## 配置方式

### 1. GLM-4.5 通过 Codex 代理

这是推荐的配置方式，因为 GLM 提供了完整的 OpenAI 兼容接口。

#### Rust 代码配置

```rust
use ai_agents::{AgentConfig, AgentType, ModelProviderConfig};

let config = AgentConfig {
    agent_type: AgentType::Codex,
    model: "GLM-4.5".to_string(),
    provider: ModelProviderConfig::glm(),
    reasoning_effort: "high".to_string(),
    preferred_auth_method: "apikey".to_string(),
    ..AgentConfig::default()
};
```

#### 环境变量设置

```bash
export GLM_AUTH_TOKEN=your-glm-token
```

#### 对应的 Codex TOML 配置

如果你想手动配置 Codex CLI，可以参考以下配置：

```toml
# ~/.codex/config.toml
model_provider = "glm"
model = "GLM-4.5"
model_reasoning_effort = "high"
preferred_auth_method = "apikey"

[model_providers.glm]
name = "glm"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
env_key = "GLM_AUTH_TOKEN"
requires_openai_auth = false

[projects."/your/project/path"]
trust_level = "trusted"
```

#### 自动环境变量配置

当使用 `ai-agents` 库时，以下环境变量会自动设置：

```bash
OPENAI_API_KEY=your-glm-token
OPENAI_BASE_URL=https://open.bigmodel.cn/api/coding/paas/v4
```

### 2. GLM-4.5 通过 Claude Code 代理

#### Rust 代码配置

```rust
use ai_agents::{AgentConfig, AgentType, ModelProviderConfig};

let config = AgentConfig {
    agent_type: AgentType::Claude,
    model: "GLM-4.5".to_string(),
    provider: ModelProviderConfig::glm_anthropic(),
    reasoning_effort: "high".to_string(),
    preferred_auth_method: "apikey".to_string(),
    ..AgentConfig::default()
};
```

#### 环境变量设置

**Bash/Zsh:**
```bash
export GLM_AUTH_TOKEN=your-glm-token
```

**Fish Shell:**
```fish
set -Ux GLM_AUTH_TOKEN your-glm-token
```

#### 自动环境变量配置

当使用 `ai-agents` 库时，以下环境变量会自动设置：

```bash
ANTHROPIC_AUTH_TOKEN=your-glm-token
ANTHROPIC_BASE_URL=https://open.bigmodel.cn/api/anthropic
ANTHROPIC_MODEL=GLM-4.5
ANTHROPIC_SMALL_FAST_MODEL=GLM-4.5-Air
```

### 3. 标准 OpenAI 配置

#### Rust 代码配置

```rust
let config = AgentConfig {
    agent_type: AgentType::Codex,
    model: "gpt-4".to_string(),
    provider: ModelProviderConfig::openai(),
    ..AgentConfig::default()
};
```

#### 环境变量

```bash
export OPENAI_API_KEY=your-openai-key
```

### 4. 标准 Claude 配置

#### Rust 代码配置

```rust
let config = AgentConfig {
    agent_type: AgentType::Claude,
    model: "claude-3-5-sonnet-20241022".to_string(),
    provider: ModelProviderConfig::claude(),
    ..AgentConfig::default()
};
```

#### 环境变量

```bash
export ANTHROPIC_API_KEY=your-claude-key
```

## 预定义提供商配置

### ModelProviderConfig 方法

```rust
// GLM 配置（通过 OpenAI 兼容接口）
let glm_provider = ModelProviderConfig::glm();

// GLM 配置（通过 Anthropic 兼容接口）
let glm_anthropic_provider = ModelProviderConfig::glm_anthropic();

// 标准 OpenAI 配置
let openai_provider = ModelProviderConfig::openai();

// 标准 Claude 配置
let claude_provider = ModelProviderConfig::claude();
```

### 自定义提供商配置

```rust
let custom_provider = ModelProviderConfig {
    name: "custom".to_string(),
    base_url: "https://your-api.com/v1".to_string(),
    env_key: "CUSTOM_API_KEY".to_string(),
    requires_openai_auth: true,
    extra_params: {
        let mut params = std::collections::HashMap::new();
        params.insert("CUSTOM_MODEL".to_string(), "your-model".to_string());
        params
    },
};
```

## 完整示例

### 使用 GLM-4.5 的完整示例

```rust
use ai_agents::{AgentManagerBuilder, AgentConfig, AgentType, ModelProviderConfig, ManagedAgent};
use agent_client_protocol::{Agent, InitializeRequest, PromptRequest};
use tokio::sync::mpsc;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::init();

    // 创建通信通道
    let (session_tx, _session_rx) = mpsc::unbounded_channel();
    let (client_tx, _client_rx) = mpsc::unbounded_channel();

    // 配置 GLM-4.5 通过 Codex 代理
    let config = AgentConfig {
        agent_type: AgentType::Codex,
        model: "GLM-4.5".to_string(),
        provider: ModelProviderConfig::glm(),
        reasoning_effort: "high".to_string(),
        preferred_auth_method: "apikey".to_string(),
        ..AgentConfig::default()
    };

    // 创建代理管理器
    let manager = AgentManagerBuilder::new()
        .with_config(config)
        .build(session_tx, client_tx)?;

    let manager = Arc::new(tokio::sync::RwLock::new(manager));
    let agent = ManagedAgent::new(manager);

    // 初始化代理
    let init_response = agent.initialize(InitializeRequest {
        protocol_version: agent_client_protocol::V1,
        client_capabilities: Default::default(),
        meta: None,
    }).await?;

    println!("代理初始化成功: {:?}", init_response.auth_methods);

    // 可以开始使用代理进行对话...
    Ok(())
}
```

## 环境变量总结

| 模型 | 代理 | 需要设置的环境变量 | 自动设置的环境变量 |
|------|------|-------------------|-------------------|
| GLM-4.5 | Codex | `GLM_AUTH_TOKEN` | `OPENAI_API_KEY`, `OPENAI_BASE_URL` |
| GLM-4.5 | Claude | `GLM_AUTH_TOKEN` | `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL` |
| GPT-4 | Codex | `OPENAI_API_KEY` | 无 |
| Claude | Claude | `ANTHROPIC_API_KEY` | 无 |

## 故障排除

### 常见问题

1. **代理启动失败**
   - 检查对应的环境变量是否设置
   - 验证 API 令牌是否有效
   - 确认网络连接到对应的 API 端点

2. **模型调用失败**
   - 确认模型名称正确
   - 检查 API 配额是否充足
   - 验证 Base URL 是否正确

3. **权限错误**
   - 检查 API 密钥是否有访问相应模型的权限
   - 确认账户状态正常

### 调试方法

启用详细日志：

```rust
// 在程序开始时添加
tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .init();
```

查看详细的配置信息：

```rust
println!("当前代理类型: {:?}", manager.current_agent_type());
println!("可用代理: {:?}", manager.available_agents());
```

## 扩展支持

要添加新的模型提供商，可以创建自定义的 `ModelProviderConfig`：

```rust
impl ModelProviderConfig {
    pub fn baichuan() -> Self {
        Self {
            name: "baichuan".to_string(),
            base_url: "https://api.baichuan-ai.com/v1".to_string(),
            env_key: "BAICHUAN_API_KEY".to_string(),
            requires_openai_auth: true,
            extra_params: std::collections::HashMap::new(),
        }
    }
}
```

这样就可以支持更多的国内大模型服务了。