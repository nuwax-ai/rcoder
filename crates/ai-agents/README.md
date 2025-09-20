# AI Agents 统一管理库

这个库提供了一个统一的接口来管理和使用不同的 AI 代理工具（Claude、Codex 等），通过 Agent Client Protocol (ACP) 提供透明的访问。

## 特性

- 🤖 **统一接口**: 通过相同的 API 访问不同的 AI 代理
- 🔄 **动态切换**: 运行时切换不同的 AI 代理后端
- 🚀 **自动检测**: 自动检测并注册可用的 AI 代理
- ⚙️ **灵活配置**: 支持每个代理的独立配置
- 🔌 **可扩展**: 易于添加新的 AI 代理支持

## 支持的 AI 代理

| 代理类型 | 描述 | 环境要求 |
|---------|------|----------|
| Claude | Anthropic Claude Code | `CLAUDE_API_KEY` 环境变量 |
| Codex | OpenAI Codex/GPT | `OPENAI_API_KEY` 环境变量 |

## 快速开始

### 基本使用

```rust
use ai_agents::{AgentManagerBuilder, AgentConfig, AgentType, ManagedAgent};
use agent_client_protocol::{Agent, InitializeRequest};
use tokio::sync::mpsc;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 创建通信通道
    let (session_tx, _session_rx) = mpsc::unbounded_channel();
    let (client_tx, _client_rx) = mpsc::unbounded_channel();

    // 配置并创建代理管理器
    let manager = AgentManagerBuilder::new()
        .with_preferred_agents(vec![AgentType::Codex, AgentType::Claude])  // 优先使用 Codex 支持 GLM
        .build(session_tx, client_tx)?;

    let manager = Arc::new(tokio::sync::RwLock::new(manager));
    let agent = ManagedAgent::new(manager);

    // 初始化代理
    let response = agent.initialize(InitializeRequest {
        protocol_version: agent_client_protocol::V1,
        client_capabilities: Default::default(),
        meta: None,
    }).await?;

    println!("代理初始化成功: {:?}", response);
    Ok(())
}
```

### 手动管理代理

```rust
use ai_agents::{AgentManager, AgentConfig, AgentType};

// 创建管理器
let mut manager = AgentManager::new(session_tx);

// 检查代理可用性
if AgentManager::is_agent_available(AgentType::Claude) {
    let config = AgentConfig {
        agent_type: AgentType::Claude,
        cwd: std::env::current_dir()?,
        model: "claude-3-5-sonnet-20241022".to_string(),
        // ... 其他配置
    };
    
    manager.register_agent(AgentType::Claude, config, client_tx)?;
}

// 切换代理
manager.switch_agent(AgentType::Codex)?;
```

### 自动注册代理

```rust
// 自动检测并注册所有可用的代理
let base_config = AgentConfig::default();
let registered = manager.auto_register_agents(base_config, client_tx)?;
println!("已注册的代理: {:?}", registered);
```

## 配置

### AgentConfig 结构

```rust
pub struct AgentConfig {
    /// 代理类型
    pub agent_type: AgentType,
    /// 工作目录
    pub cwd: std::path::PathBuf,
    /// 代理主目录
    pub home_dir: std::path::PathBuf,
    /// 使用的模型
    pub model: String,
    /// 额外的环境变量
    pub env_vars: std::collections::HashMap<String, String>,
}
```

### Claude 配置示例

```rust
let claude_config = AgentConfig {
    agent_type: AgentType::Claude,
    cwd: std::path::PathBuf::from("/path/to/project"),
    home_dir: std::path::PathBuf::from("/home/user/.claude"),
    model: "claude-3-5-sonnet-20241022".to_string(),
    env_vars: {
        let mut env = std::collections::HashMap::new();
        env.insert("CLAUDE_API_KEY".to_string(), "your-api-key".to_string());
        env
    },
};
```

### Codex 配置示例（支持 GLM-4.5）

```rust
let codex_config = AgentConfig {
    agent_type: AgentType::Codex,
    cwd: std::path::PathBuf::from("/path/to/project"),
    home_dir: std::path::PathBuf::from("/home/user/.codex"),
    model: "GLM-4.5".to_string(),  // 使用 GLM-4.5 模型
    env_vars: {
        let mut env = std::collections::HashMap::new();
        env.insert("GLM_AUTH_TOKEN".to_string(), "your-glm-token".to_string());
        env
    },
};
```

## API 参考

### AgentManager

主要的代理管理器，提供以下方法：

- `new()`: 创建新的代理管理器
- `register_agent()`: 注册特定类型的代理
- `switch_agent()`: 切换当前活动代理
- `current_agent()`: 获取当前代理引用
- `available_agents()`: 获取所有可用代理类型
- `auto_register_agents()`: 自动注册可用代理

### AgentManagerBuilder

用于构建代理管理器的构建器模式：

- `new()`: 创建新的构建器
- `with_config()`: 设置基础配置
- `with_preferred_agents()`: 设置偏好的代理列表
- `build()`: 构建最终的管理器

### ManagedAgent

实现 `Agent` trait 的包装器，透明地代理到当前选择的代理。

## 环境设置

### Claude

```bash
export CLAUDE_API_KEY="your-claude-api-key"
```

### Codex/OpenAI

```bash
export OPENAI_API_KEY="your-openai-api-key"
```

## 示例

查看 `examples/agent_usage.rs` 获取完整的使用示例。

运行示例：

```bash
cargo run --example agent_usage
```

## 错误处理

库使用 `agent_client_protocol::Error` 作为主要错误类型。常见错误：

- `invalid_params`: 参数无效（如代理类型未注册）
- `internal_error`: 内部错误（如代理初始化失败）
- `auth_required`: 需要认证（如 API 密钥未设置）

## 扩展

要添加新的 AI 代理支持：

1. 在 `AgentType` 枚举中添加新类型
2. 在 `AgentManager::register_agent()` 中添加创建逻辑
3. 在 `AgentManager::is_agent_available()` 中添加可用性检查
4. 在 `AgentManager::get_auth_methods()` 中添加认证方法

## 许可证

MIT OR Apache-2.0