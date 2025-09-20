# AI 代理重构总结

## 概述

本次重构将 `claude` 和 `codex` crates 从可执行程序改为库形式，并创建了一个统一的 AI 代理管理库 `ai-agents`，提供了透明的接口来使用不同的 AI 工具。**重要说明**：`AgentType` 代表的是代理工具（如 Claude Code、Codex），而不是具体的模型。GLM-4.5 等模型可以通过 Codex 或 Claude Code 工具来使用。

## 主要改动

### 1. Claude Crate 重构

**文件变更：**
- `crates/claude/Cargo.toml` - 添加了 `[lib]` 配置
- `crates/claude/src/lib.rs` - 新增，作为库的入口点
- `crates/claude/src/main.rs` - 保留，可选择性编译

**新增功能：**
- `ClaudeAgentFactory` - 工厂模式创建 Claude 代理
- `ClaudeAgentBuilder` - 构建器模式配置 Claude 代理
- 支持检查 Claude 可用性和认证方法

### 2. Codex Crate 重构

**文件变更：**
- `crates/codex/Cargo.toml` - 添加了 `[lib]` 配置，注释了不存在的依赖
- `crates/codex/src/lib.rs` - 新增，作为库的入口点
- `crates/codex/src/main.rs` - 修复了不存在的依赖引用

**新增功能：**
- `CodexAgentFactory` - 工厂模式创建 Codex 代理
- `CodexAgentBuilder` - 构建器模式配置 Codex 代理
- 支持检查 Codex 可用性和认证方法

### 3. 新增 AI Agents 统一管理库

**新建文件：**
- `crates/ai-agents/Cargo.toml` - 新 crate 配置
- `crates/ai-agents/src/lib.rs` - 统一管理库核心代码
- `crates/ai-agents/README.md` - 详细使用文档

**核心功能：**

#### AgentType 枚举
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentType {
    Claude,  // Claude Code 代理工具
    Codex,   // OpenAI Codex 代理工具
}
```

**重要说明**：
- `AgentType::Codex` 可以使用多种模型：GLM-4.5、GPT-4、GPT-3.5 等
- `AgentType::Claude` 可以使用 Claude 系列模型：claude-3-5-sonnet-20241022 等
- 模型配置通过 `AgentConfig.model` 字段指定

#### AgentManager 统一管理器
- 注册和管理多个 AI 代理
- 运行时切换不同代理
- 自动检测和注册可用代理
- 提供统一的配置接口

#### ManagedAgent 透明代理
- 实现 `Agent` trait
- 透明地代理到当前选择的代理
- 其他模块无需关心底层使用的具体代理

#### AgentManagerBuilder 构建器
- 便捷的链式配置
- 支持偏好代理列表
- 自动回退机制

## 使用方式

### 基本使用

```rust
use ai_agents::{AgentManagerBuilder, AgentType, ManagedAgent};

// 创建管理器，自动选择可用的代理
let manager = AgentManagerBuilder::new()
    .with_preferred_agents(vec![AgentType::Claude, AgentType::Codex])
    .build(session_tx, client_tx)?;

let manager = Arc::new(tokio::sync::RwLock::new(manager));
let agent = ManagedAgent::new(manager);

// 使用统一的 Agent 接口
let response = agent.initialize(request).await?;
```

### 手动管理代理

```rust
let mut manager = AgentManager::new(session_tx);

// 检查可用性
if AgentManager::is_agent_available(AgentType::Claude) {
    manager.register_agent(AgentType::Claude, config, client_tx)?;
}

// 切换代理
manager.switch_agent(AgentType::Codex)?;
```

### 自动注册

```rust
// 自动检测并注册所有可用的代理
let registered = manager.auto_register_agents(base_config, client_tx)?;
```

## 架构优势

### 1. 透明性
- 其他模块通过统一的 `Agent` trait 接口使用
- 无需关心底层使用的是 Claude 还是 Codex
- 可以在运行时动态切换代理

### 2. 可扩展性
- 新增 AI 代理只需：
  1. 在 `AgentType` 枚举中添加新类型
  2. 在管理器中添加创建逻辑
  3. 实现可用性检查

### 3. 配置灵活性
- 每个代理支持独立配置
- 支持环境变量和配置文件
- 构建器模式提供链式配置

### 4. 错误处理
- 统一的错误类型 `agent_client_protocol::Error`
- 优雅的回退机制
- 详细的错误信息

## 配置

### AgentConfig 结构

```rust
pub struct AgentConfig {
    /// 代理类型（Claude Code 或 Codex 工具）
    pub agent_type: AgentType,
    /// 工作目录
    pub cwd: std::path::PathBuf,
    /// 代理主目录
    pub home_dir: std::path::PathBuf,
    /// 使用的模型（如 GLM-4.5、claude-3-5-sonnet-20241022、gpt-4 等）
    pub model: String,
    /// 额外的环境变量
    pub env_vars: std::collections::HashMap<String, String>,
}
```

### GLM-4.5 配置示例（通过 Codex 使用）

```rust
let glm_config = AgentConfig {
    agent_type: AgentType::Codex,  // 使用 Codex 代理工具
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

## 环境设置

### GLM-4.5 （通过 Codex）

```bash
export GLM_AUTH_TOKEN="your-glm-token"
```

### Claude

```bash
export CLAUDE_API_KEY="your-claude-api-key"
```

### Codex/OpenAI

```bash
export OPENAI_API_KEY="your-openai-api-key"
```

## 示例和测试

### 运行示例
```bash
cargo run --example agent_usage
```

### 编译测试
```bash
cargo check --package claude --package codex --package ai-agents
```

## 文件结构

```
crates/
├── ai-agents/              # 统一管理库
│   ├── src/
│   │   └── lib.rs         # 核心管理逻辑
│   ├── Cargo.toml
│   └── README.md
├── claude/                 # Claude 代理库
│   ├── src/
│   │   ├── lib.rs         # 库入口点
│   │   ├── main.rs        # 可执行程序（可选）
│   │   └── agent/
│   └── Cargo.toml
├── codex/                  # Codex 代理库
│   ├── src/
│   │   ├── lib.rs         # 库入口点
│   │   ├── main.rs        # 可执行程序（可选）
│   │   └── agent/
│   └── Cargo.toml
└── acp_adapter/            # ACP 协议适配器
    └── ...
examples/
└── agent_usage.rs          # 使用示例
```

## 后续工作

1. **完善测试**：添加单元测试和集成测试
2. **性能优化**：优化代理切换和初始化性能
3. **模型支持**：扩展对更多模型的支持（如通过不同的 provider）
4. **配置管理**：支持配置文件和环境变量管理
5. **监控支持**：添加代理状态监控和度量

## 兼容性

- 保持与原有 ACP 协议的完全兼容
- 支持现有的 Claude Code 和 OpenAI API
- 支持智谱 GLM 模型（通过 Codex 工具）
- 向后兼容原有的代理接口

这次重构实现了你要求的所有功能：
1. ✅ 将 claude 和 codex 改为 lib
2. ✅ 通过 lib 暴露服务
3. ✅ 统一的 agent_client_protocol 接口
4. ✅ 透明使用，无需关心底层实现
5. ✅ 通过 enum 控制使用哪个具体代理工具
6. ✅ 支持 GLM-4.5 模型配置（通过 Codex 代理）