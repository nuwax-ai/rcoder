# 国内大模型集成指南

本文档说明如何在 ACP 适配器中配置和使用国内大模型。

## 🏗️ 架构支持

现在 ACP 适配器支持两种配置方式：

### 1. 国际模型（如 Claude Code）
```
你的 Rust 应用
    ↓ ACP 协议
你的 acp-adapter (Rust)
    ↓ 进程调用
@zed-industries/claude-code-acp (Node.js 适配器)
    ↓ 调用本地命令
claude 命令 (你本地安装的)
    ↓ 使用你的配置
Anthropic API
```

### 2. 国内大模型
```
你的 Rust 应用
    ↓ ACP 协议
你的 acp-adapter (Rust)
    ↓ 进程调用
你的自定义脚本/程序
    ↓ 使用 base_url 和 api_key
国内大模型 API
```

## 🔧 配置方法

### 方法一：直接配置

```rust
use acp_adapter::{AcpAdapter, AcpConfig};

// 直接配置国内大模型
let config = AcpConfig::domestic_model(
    "claude".to_string(),                    // 代理类型
    "python".to_string(),                   // 命令
    "your_domestic_api_key".to_string(),    // API Key
    "https://api.domestic-provider.com/v1".to_string(), // Base URL
);

let adapter = AcpAdapter::new(config);
adapter.initialize().await?;
```

### 方法二：环境变量配置

```bash
# 设置环境变量
export API_KEY="your_domestic_api_key"
export BASE_URL="https://api.domestic-provider.com/v1"

# 或者使用备用环境变量名
export DOMESTIC_API_KEY="your_domestic_api_key"
export DOMESTIC_BASE_URL="https://api.domestic-provider.com/v1"
```

```rust
use acp_adapter::{AcpAdapter, AcpConfig};

// 从环境变量创建配置
let config = AcpConfig::domestic_model_from_env(
    "claude".to_string(),
    "python".to_string(),
)?;

let adapter = AcpAdapter::new(config);
adapter.initialize().await?;
```

### 方法三：完全自定义配置

```rust
use acp_adapter::{AcpAdapter, AcpConfig};

let config = AcpConfig::new("custom_model".to_string(), "custom_command".to_string())
    .with_custom_api_key_auth(
        "custom_key".to_string(),
        Some("X-API-Key".to_string()),  // 自定义 header
        Some("https://custom.api.com".to_string()),
    )
    .with_working_dir(PathBuf::from("."))
    .with_timeout(120);

let adapter = AcpAdapter::new(config);
adapter.initialize().await?;
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

## 🚀 使用示例

### 运行示例

```bash
# 运行国内大模型示例
cargo run --package claude_code_acp_example --bin domestic_model_example

# 设置环境变量后运行
export API_KEY="your_domestic_api_key"
export BASE_URL="https://api.domestic-provider.com/v1"
cargo run --package claude_code_acp_example --bin domestic_model_example
```

### 完整示例

```rust
use acp_adapter::{AcpAdapter, AcpConfig};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    // 创建国内大模型配置
    let config = AcpConfig::domestic_model(
        "claude".to_string(),
        "python".to_string(),
        "your_domestic_api_key".to_string(),
        "https://api.domestic-provider.com/v1".to_string(),
    )
    .with_working_dir(PathBuf::from("."))
    .with_timeout(120);

    // 创建适配器
    let adapter = AcpAdapter::new(config);
    
    // 初始化
    adapter.initialize().await?;
    
    // 创建会话
    let session = adapter.create_session().await?;
    
    println!("✅ 国内大模型会话创建成功: {}", session.id());
    
    Ok(())
}
```

## 🔍 配置参数说明

### AuthenticationMethod 枚举

```rust
pub enum AuthenticationMethod {
    None,
    ApiKey {
        key: String,                    // API 密钥
        header_name: Option<String>,    // 请求头名称（默认: "Authorization"）
        base_url: Option<String>,       // 基础 URL
    },
    OAuth {
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
        base_url: Option<String>,       // 基础 URL
    },
    Custom {
        name: String,
        parameters: HashMap<String, String>,
    },
}
```

### 配置方法

| 方法 | 描述 | 参数 |
|------|------|------|
| `domestic_model()` | 创建国内大模型配置 | agent_type, command, api_key, base_url |
| `domestic_model_from_env()` | 从环境变量创建配置 | agent_type, command |
| `with_api_key_auth()` | 设置 API Key 认证 | key |
| `with_api_key_auth_and_url()` | 设置 API Key 认证（带 URL） | key, base_url |
| `with_custom_api_key_auth()` | 完全自定义 API Key 认证 | key, header_name, base_url |

## 🎯 最佳实践

### 1. 环境变量管理
```bash
# 创建 .env 文件
echo "API_KEY=your_domestic_api_key" >> .env
echo "BASE_URL=https://api.domestic-provider.com/v1" >> .env

# 在代码中加载
dotenv::dotenv().ok();
```

### 2. 配置验证
```rust
// 验证配置
config.validate()?;

// 检查必需的环境变量
if std::env::var("API_KEY").is_err() {
    return Err("请设置 API_KEY 环境变量".into());
}
```

### 3. 错误处理
```rust
match AcpConfig::domestic_model_from_env("claude".to_string(), "python".to_string()) {
    Ok(config) => {
        // 配置成功
    }
    Err(e) => {
        eprintln!("配置失败: {}", e);
        eprintln!("请确保设置了 API_KEY 和 BASE_URL 环境变量");
    }
}
```

## 🔧 故障排除

### 常见问题

1. **环境变量未设置**
   ```bash
   # 检查环境变量
   echo $API_KEY
   echo $BASE_URL
   ```

2. **API Key 无效**
   ```bash
   # 测试 API Key
   curl -H "Authorization: Bearer $API_KEY" "$BASE_URL/models"
   ```

3. **Base URL 错误**
   ```bash
   # 检查 Base URL 格式
   curl -I "$BASE_URL"
   ```

### 调试模式

```bash
# 启用详细日志
RUST_LOG=debug cargo run --package claude_code_acp_example --bin domestic_model_example

# 或者
RUST_LOG=acp_adapter=debug cargo run --package claude_code_acp_example --bin domestic_model_example
```

## 📊 总结

现在你的 ACP 适配器支持：

- ✅ **国际模型**：通过 `claude-code-acp` 调用本地 Claude Code
- ✅ **国内大模型**：通过 `base_url` 和 `api_key` 直接调用
- ✅ **灵活配置**：支持多种配置方式
- ✅ **环境变量**：支持从环境变量读取配置
- ✅ **完全自定义**：支持自定义 header 和 URL

这样你就可以根据需要使用不同的模型提供商了！
