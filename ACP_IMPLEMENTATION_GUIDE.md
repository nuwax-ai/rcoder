# ACP 协议实现指南

本文档描述了基于对 `agent-client-protocol` 源码深入分析后实现的真正 ACP 协议通信方案。

## 概述

基于对 `/Volumes/soddy/git_workspace/rcoder/tmp/agent-client-protocol` 目录中 ACP 协议源码的深入分析，我们实现了完整的 ACP 协议客户端，支持与 codex agent 的真正通信。

## ACP 协议架构分析

### 核心组件

1. **Agent** (`agent.rs`): AI 编码代理，处理客户端请求
2. **Client** (`client.rs`): 客户端，处理代理请求并提供资源访问
3. **连接类型**:
   - `ClientSideConnection`: 客户端视角的连接
   - `AgentSideConnection`: 代理视角的连接

### 协议流程

1. **初始化**: 协商协议版本和能力
2. **认证** (可选): 如果代理需要认证
3. **创建会话**: 建立独立的对话上下文
4. **发送提示**: 通过 `session/prompt` 发送用户输入
5. **接收响应**: 通过 `session/update` 通知获取实时响应

## 实现文件

### 1. `acp_client.rs` - 核心 ACP 客户端实现

包含完整的 ACP 协议客户端实现：

- `AcpClientConfig`: ACP 客户端配置
- `AcpClient`: 主要的客户端实现
- `AcpConnectionManager`: 连接管理器
- `AcpClientHandler`: Client trait 实现

**主要特性**:
- 完整的 ACP 协议支持
- 自动连接管理
- 错误处理和重试逻辑
- 会话管理
- 实时响应处理

### 2. 更新的 `codex_acp_mpmc.rs`

集成了真正的 ACP 协议通信：

- 使用 `AcpConnectionManager` 替代模拟实现
- 支持真正的 codex agent 通信
- 保持原有的 MPMC 架构
- 改进的错误处理和状态管理

### 3. `real_acp_example.rs` - 完整示例

展示如何使用真正的 ACP 协议：

- 直接 ACP 客户端使用
- 连接管理器使用
- 全局 MPMC 管理器使用
- 错误处理示例
- 性能测试示例

## 使用方法

### 1. 基本使用

```rust
use rcoder::acp_client::{AcpClientConfig, AcpClient};

#[tokio::main]
async fn main() -> Result<()> {
    // 创建配置
    let config = AcpClientConfig::for_codex()
        .with_api_key("your-api-key".to_string())
        .with_working_dir(PathBuf::from("./project"));

    // 创建客户端
    let mut client = AcpClient::new(config);

    // 初始化连接
    client.initialize().await?;

    // 创建会话
    let session = client.create_session().await?;

    // 发送提示
    let response = client.send_prompt("Hello, codex!").await?;
    println!("Response: {}", response);

    Ok(())
}
```

### 2. 使用连接管理器

```rust
use rcoder::acp_client::{AcpClientConfig, AcpConnectionManager};

#[tokio::main]
async fn main() -> Result<()> {
    let config = AcpClientConfig::for_codex()
        .with_api_key("your-api-key".to_string())
        .with_working_dir(PathBuf::from("."));

    let mut manager = AcpConnectionManager::new(config);

    // 自动管理连接和会话
    let response = manager.send_prompt("Hello, codex!").await?;
    println!("Response: {}", response);

    Ok(())
}
```

### 3. 使用全局 MPMC 管理器

```rust
use rcoder::codex_acp_mpmc::send_prompt_global;

#[tokio::main]
async fn main() -> Result<()> {
    let response = send_prompt_global("project1", "Hello, codex!").await?;
    println!("Response: {}", response);
    Ok(())
}
```

## 前置条件

### 1. 安装 claude-code

```bash
npm install -g @anthropic-ai/claude-code
```

### 2. 设置环境变量

```bash
export ANTHROPIC_API_KEY="your-api-key-here"
```

### 3. 验证安装

```bash
claude-code --version
```

## 配置选项

### AcpClientConfig

```rust
let config = AcpClientConfig::for_codex()
    .with_working_dir(PathBuf::from("./project"))
    .with_api_key("your-api-key".to_string())
    .with_env("CUSTOM_VAR".to_string(), "value".to_string());
```

### 客户端能力

```rust
let mut config = AcpClientConfig::default();
config.client_capabilities.fs.read_text_file = true;
config.client_capabilities.fs.write_text_file = true;
config.client_capabilities.terminal = true;
```

## 错误处理

### 常见错误及解决方案

1. **连接失败**
   - 检查 claude-code 是否已安装
   - 验证 API 密钥是否正确
   - 确保网络连接正常

2. **认证失败**
   - 检查 ANTHROPIC_API_KEY 环境变量
   - 验证 API 密钥有效性

3. **会话创建失败**
   - 检查工作目录是否存在
   - 确保有足够的文件系统权限

### 错误处理示例

```rust
match client.initialize().await {
    Ok(_) => {
        // 连接成功
    }
    Err(e) => {
        eprintln!("连接失败: {}", e);
        eprintln!("请检查 claude-code 安装和 API 密钥设置");
    }
}
```

## 性能优化

### 1. 连接复用

使用 `AcpConnectionManager` 自动管理连接生命周期：

```rust
let mut manager = AcpConnectionManager::new(config);
// 连接会自动创建和复用
let response1 = manager.send_prompt("Hello").await?;
let response2 = manager.send_prompt("How are you?").await?;
```

### 2. 全局管理器

对于多项目场景，使用全局 MPMC 管理器：

```rust
// 每个项目一个服务实例
let response1 = send_prompt_global("project1", "Hello").await?;
let response2 = send_prompt_global("project2", "Hello").await?;
```

## 日志和调试

### 启用详细日志

```rust
tracing_subscriber::fmt::init();
```

### 关键日志信息

- 连接建立状态
- 会话创建过程
- 提示发送和接收
- 错误详情

## 测试

### 运行示例

```bash
# 设置环境变量
export ANTHROPIC_API_KEY="your-api-key"

# 运行示例
cargo run --example real_acp_example
```

### 单元测试

```bash
cargo test acp_client
```

## 架构设计

### 类图

```
AcpClientConfig
    ↓
AcpClient ←→ AcpConnectionManager
    ↓            ↓
AcpClientHandler  GlobalCodexManager (MPMC)
    ↓
Codex Agent (claude-code)
```

### 数据流

1. **初始化**: Client → Agent (initialize)
2. **会话创建**: Client → Agent (session/new)
3. **提示发送**: Client → Agent (session/prompt)
4. **实时响应**: Agent → Client (session/update)
5. **工具调用**: Agent → Client (各种工具请求)

## 协议细节

### JSON-RPC 消息格式

ACP 协议基于 JSON-RPC 2.0，所有消息都遵循此格式。

### 关键方法

- `initialize`: 协议初始化
- `session/new`: 创建新会话
- `session/prompt`: 发送提示
- `session/update`: 会话更新通知
- `session/cancel`: 取消操作

### 内容块类型

- `Text`: 文本内容
- `Image`: 图像内容
- `Audio`: 音频内容
- `ResourceLink`: 资源链接
- `Resource`: 嵌入资源

## 故障排除

### 常见问题

1. **claude-code 命令未找到**
   ```bash
   npm install -g @anthropic-ai/claude-code
   ```

2. **API 密钥无效**
   ```bash
   export ANTHROPIC_API_KEY="your-valid-api-key"
   ```

3. **网络连接问题**
   - 检查防火墙设置
   - 验证代理配置

4. **权限问题**
   - 确保对工作目录有读写权限
   - 检查文件系统权限

### 调试技巧

1. **启用详细日志**
   ```rust
   tracing_subscriber::fmt::with_max_level(tracing::Level::DEBUG).init();
   ```

2. **检查连接状态**
   ```rust
   println!("Connected: {}", client.is_connected());
   ```

3. **验证配置**
   ```rust
   println!("Config: {:?}", client.config);
   ```

## 扩展功能

### 自定义 Client trait 实现

可以通过实现 `Client` trait 来自定义客户端行为：

```rust
#[async_trait::async_trait(?Send)]
impl Client for MyCustomClient {
    async fn request_permission(&self, args: RequestPermissionRequest) -> Result<RequestPermissionResponse, acp::Error> {
        // 自定义权限处理逻辑
    }

    // 实现其他必要方法...
}
```

### 支持其他 Agent

ACP 协议设计支持多种 AI 编码代理，不仅限于 claude-code：

```rust
let config = AcpClientConfig {
    codex_command: "other-agent".to_string(),
    codex_args: vec!["--stdio".to_string()],
    // ... 其他配置
};
```

## 总结

这个实现提供了：

✅ **完整的 ACP 协议支持**: 基于 agent-client-protocol 源码分析
✅ **真正的代理通信**: 与 claude-code 等代理进行实际通信
✅ **多种使用模式**: 直接客户端、连接管理器、全局 MPMC
✅ **错误处理**: 完善的错误处理和重试逻辑
✅ **会话管理**: 自动会话创建和管理
✅ **性能优化**: 连接复用和多项目支持
✅ **易于扩展**: 支持自定义实现和其他代理

这个实现解决了之前模拟实现的限制，提供了真正的 ACP 协议通信能力。