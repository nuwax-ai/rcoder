# 自定义模型配置功能

## 概述

已成功将 `ai-agents` 模块删除，并在 `acp_adapter` 的 `config.rs` 中增加了对自定义模型的支持。现在用户可以通过简单的配置来使用各种大模型提供商，包括国内的大模型。

## 主要功能

### 1. 模型提供商配置 (`ModelProviderConfig`)

支持多种预定义的模型提供商：

- **GLM (智谱AI)**: `ModelProviderConfig::glm()`
- **Claude (Anthropic)**: `ModelProviderConfig::claude()`
- **OpenAI**: `ModelProviderConfig::openai()`
- **通义千问 (阿里云)**: `ModelProviderConfig::qwen()`
- **文心一言 (百度)**: `ModelProviderConfig::ernie()`
- **月之暗面**: `ModelProviderConfig::moonshot()`
- **自定义提供商**: `ModelProviderConfig::custom()`

### 2. 简化的配置方法

#### 预定义配置
```rust
// GLM 配置
let config = AcpConfig::glm("claude".to_string(), "python".to_string());

// 通义千问配置
let config = AcpConfig::qwen("claude".to_string(), "python".to_string());

// 文心一言配置
let config = AcpConfig::ernie("claude".to_string(), "python".to_string());
```

#### 自定义模型配置
```rust
let config = AcpConfig::custom_model(
    "claude".to_string(),           // 代理类型
    "python".to_string(),           // 命令
    "my_provider".to_string(),      // 提供商名称
    "https://api.my-provider.com/v1".to_string(), // Base URL
    "MY_PROVIDER_API_KEY".to_string(), // 环境变量名
    "my-custom-model".to_string(),  // 模型名称
);
```

#### 手动配置
```rust
let config = AcpConfig::new("claude".to_string(), "python".to_string())
    .with_model_provider(ModelProviderConfig::moonshot())
    .with_model_name("moonshot-v1-32k".to_string());
```

### 3. 环境变量自动生成

配置会自动生成相应的环境变量：

```rust
let env_vars = config.full_environment_with_provider();
// 对于 OpenAI 兼容的提供商，会生成：
// - OPENAI_API_KEY
// - OPENAI_BASE_URL
// 对于自定义提供商，会生成：
// - {PROVIDER_NAME}_API_KEY
// - {PROVIDER_NAME}_BASE_URL
```

## 使用示例

### 示例1: 使用 GLM 模型
```rust
use acp_adapter::AcpConfig;

let config = AcpConfig::glm("claude".to_string(), "python".to_string())
    .with_working_dir(PathBuf::from("."))
    .with_timeout(120);

// 需要设置环境变量: export GLM_AUTH_TOKEN=your_key
```

### 示例2: 使用通义千问
```rust
let config = AcpConfig::qwen("claude".to_string(), "python".to_string())
    .with_working_dir(PathBuf::from("."))
    .with_timeout(120);

// 需要设置环境变量: export QWEN_API_KEY=your_key
```

### 示例3: 自定义模型提供商
```rust
let config = AcpConfig::custom_model(
    "claude".to_string(),
    "python".to_string(),
    "my_provider".to_string(),
    "https://api.my-provider.com/v1".to_string(),
    "MY_PROVIDER_API_KEY".to_string(),
    "my-custom-model".to_string(),
);

// 需要设置环境变量: export MY_PROVIDER_API_KEY=your_key
```

## 环境变量配置

### GLM (智谱AI)
```bash
export GLM_AUTH_TOKEN=your_glm_token
```

### 通义千问 (阿里云)
```bash
export QWEN_API_KEY=your_qwen_key
```

### 文心一言 (百度)
```bash
export ERNIE_API_KEY=your_ernie_key
```

### 月之暗面
```bash
export MOONSHOT_API_KEY=your_moonshot_key
```

### OpenAI
```bash
export OPENAI_API_KEY=your_openai_key
```

### Claude (Anthropic)
```bash
export ANTHROPIC_API_KEY=your_anthropic_key
```

## 运行示例

```bash
# 运行自定义模型配置示例
cargo run --bin custom_model_example

# 运行国内大模型示例
cargo run --bin domestic_model_example

# 运行 Claude Code ACP 示例
cargo run --bin claude_code_acp_example
```

## 测试

```bash
# 运行所有测试
cargo test --package acp-adapter

# 运行特定测试
cargo test --package acp-adapter test_model_provider_config
cargo test --package acp-adapter test_custom_model_config
```

## 架构优势

1. **简化配置**: 用户只需要指定提供商和模型名称，无需手动配置复杂的参数
2. **灵活扩展**: 支持自定义提供商，可以轻松添加新的模型提供商
3. **环境变量管理**: 自动生成和管理环境变量，减少配置错误
4. **类型安全**: 使用 Rust 的类型系统确保配置的正确性
5. **向后兼容**: 保持与现有代码的兼容性

## 迁移指南

如果你之前使用 `ai-agents` 模块，现在可以：

1. 删除对 `ai-agents` 的依赖
2. 使用 `AcpConfig` 的新方法来配置模型
3. 设置相应的环境变量
4. 使用 `full_environment_with_provider()` 方法获取完整的环境变量

这样的设计让用户能够轻松地配置和使用各种大模型，无论是国外的还是国内的，都只需要简单的几行代码。
