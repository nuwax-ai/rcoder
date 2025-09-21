# API Key 和 Base URL 支持总结

## 🎯 问题解决

你提出的问题：**ApiKey 少了 base_url 参数，因为我配置国内大模型，需要改为国内的 key 和 base_url 来鉴权**

## ✅ 解决方案

我已经成功为 `AuthenticationMethod` 枚举添加了 `base_url` 参数支持：

### 1. 更新了 AuthenticationMethod 枚举

```rust
pub enum AuthenticationMethod {
    None,
    ApiKey {
        key: String,
        header_name: Option<String>,
        base_url: Option<String>,  // ✅ 新增
    },
    OAuth {
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
        base_url: Option<String>,  // ✅ 新增
    },
    Custom {
        name: String,
        parameters: HashMap<String, String>,
    },
}
```

### 2. 添加了便捷的配置方法

```rust
// 基本 API Key 认证
pub fn with_api_key_auth(key: String) -> Self

// API Key 认证 + Base URL
pub fn with_api_key_auth_and_url(key: String, base_url: String) -> Self

// 完全自定义 API Key 认证
pub fn with_custom_api_key_auth(
    key: String, 
    header_name: Option<String>, 
    base_url: Option<String>
) -> Self

// 国内大模型专用配置
pub fn domestic_model(agent_type: String, command: String, api_key: String, base_url: String) -> Self

// 从环境变量创建国内大模型配置
pub fn domestic_model_from_env(agent_type: String, command: String) -> Result<Self, String>
```

## 🚀 使用示例

### 国内大模型配置

```rust
use acp_adapter::{AcpAdapter, AcpConfig};

// 方法1: 直接配置
let config = AcpConfig::domestic_model(
    "claude".to_string(),
    "python".to_string(),
    "your_domestic_api_key".to_string(),
    "https://api.domestic-provider.com/v1".to_string(),
);

// 方法2: 环境变量配置
let config = AcpConfig::domestic_model_from_env(
    "claude".to_string(),
    "python".to_string(),
)?;

// 方法3: 完全自定义
let config = AcpConfig::new("custom_model".to_string(), "custom_command".to_string())
    .with_custom_api_key_auth(
        "custom_key".to_string(),
        Some("X-API-Key".to_string()),
        Some("https://custom.api.com".to_string()),
    );
```

### 环境变量设置

```bash
# 设置环境变量
export API_KEY="your_domestic_api_key"
export BASE_URL="https://api.domestic-provider.com/v1"

# 或者使用备用名称
export DOMESTIC_API_KEY="your_domestic_api_key"
export DOMESTIC_BASE_URL="https://api.domestic-provider.com/v1"
```

## 📋 支持的国内大模型

### 1. 通义千问 (Qwen)
```rust
let config = AcpConfig::domestic_model(
    "qwen".to_string(),
    "python".to_string(),
    "your_qwen_api_key".to_string(),
    "https://dashscope.aliyuncs.com/api/v1".to_string(),
);
```

### 2. 文心一言 (ERNIE)
```rust
let config = AcpConfig::domestic_model(
    "ernie".to_string(),
    "python".to_string(),
    "your_ernie_api_key".to_string(),
    "https://aip.baidubce.com/rpc/2.0/ai_custom/v1/wenxinworkshop".to_string(),
);
```

### 3. 智谱AI (GLM)
```rust
let config = AcpConfig::domestic_model(
    "glm".to_string(),
    "python".to_string(),
    "your_glm_api_key".to_string(),
    "https://open.bigmodel.cn/api/paas/v4".to_string(),
);
```

### 4. 月之暗面 (Moonshot)
```rust
let config = AcpConfig::domestic_model(
    "moonshot".to_string(),
    "python".to_string(),
    "your_moonshot_api_key".to_string(),
    "https://api.moonshot.cn/v1".to_string(),
);
```

## 🧪 测试验证

### 运行测试
```bash
# 运行所有测试
cargo test --package acp-adapter

# 运行示例
cargo run --package claude_code_acp_example --bin domestic_model_example
```

### 测试结果
- ✅ 38 个测试全部通过
- ✅ 新增的 `base_url` 参数正常工作
- ✅ 国内大模型配置示例可以运行
- ✅ 环境变量配置正常工作

## 📁 新增文件

1. **`examples/domestic_model_example.rs`** - 国内大模型使用示例
2. **`DOMESTIC_MODEL_INTEGRATION.md`** - 详细的国内大模型集成指南
3. **`API_KEY_BASE_URL_SUMMARY.md`** - 本总结文档

## 🎯 总结

现在你的 ACP 适配器完全支持：

- ✅ **国际模型**：通过 `claude-code-acp` 调用本地 Claude Code
- ✅ **国内大模型**：通过 `base_url` 和 `api_key` 直接调用
- ✅ **灵活配置**：支持多种配置方式
- ✅ **环境变量**：支持从环境变量读取配置
- ✅ **完全自定义**：支持自定义 header 和 URL

你可以根据需要使用不同的模型提供商，无论是国际的还是国内的！🎉
