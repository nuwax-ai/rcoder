# ACP 代理管理系统使用示例

## 概述

ACP 代理管理系统解决了 AgentSideConnection 和 ClientSideConnection 不实现 Send trait的问题，通过代理管理器模式实现在 Axum HTTP 处理器中安全使用 ACP 协议的能力。

## 核心特性

- ✅ 解决 Send trait 限制问题
- ✅ 支持多项目隔离
- ✅ 自动会话管理
- ✅ 线程安全的消息传递
- ✅ 项目工作空间管理
- ✅ 完善的错误处理

## 基本使用

### 1. 创建 ProxyAgentManager

```rust
use rcoder::proxy_agent_manager::{ProxyAgentManager, ProxyConfig};
use std::path::PathBuf;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 创建配置
    let config = ProxyConfig {
        workspace_root: PathBuf::from("./project_workspace"),
        idle_timeout: Duration::from_secs(3600), // 1小时空闲超时
        cleanup_interval: Duration::from_secs(300), // 5分钟清理间隔
        max_concurrent_agents: 10,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await?;

    // 使用管理器...
    Ok(())
}
```

### 2. 发送 Prompt 请求

```rust
// 发送 prompt（自动创建项目和会话）
let (response, session_id) = manager.send_prompt(
    None,  // 不指定 project_id，系统会自动生成
    None,  // 不指定 session_id，系统会自动创建
    "Hello, how are you?"
).await?;

println!("Response: {}", response);
println!("Session ID: {}", session_id);

// 使用现有的 session 发送 prompt
let (response, session_id) = manager.send_prompt(
    Some("existing_project_id"),
    Some(&session_id),
    "Can you help me with Rust programming?"
).await?;
```

### 3. 管理 Agent 服务

```rust
// 确保项目的 Agent 服务存在
manager.get_or_create_agent("my_project").await?;

// 清理空闲的 Agent 服务
manager.cleanup_idle_agents().await?;

// 优雅关闭
manager.shutdown().await?;
```

## 在 Axum 处理器中使用

```rust
use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize)]
struct ChatRequest {
    project_id: Option<String>,
    session_id: Option<String>,
    prompt: String,
}

#[derive(Serialize)]
struct ChatResponse {
    response: String,
    session_id: String,
    project_id: String,
}

async fn handle_chat(
    State(manager): State<Arc<ProxyAgentManager>>,
    Json(request): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, StatusCode> {
    let project_id = request.project_id
        .as_deref()
        .unwrap_or_else(|| manager.generate_project_id());

    let (response, session_id) = manager
        .send_prompt(project_id, request.session_id.as_deref(), &request.prompt)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ChatResponse {
        response,
        session_id,
        project_id: project_id.to_string(),
    }))
}
```

## 架构说明

### 核心组件

1. **ProxyAgentManager**: 主管理器，负责协调所有 Agent 服务
2. **LocalSetAgentService**: 在 LocalSet 中运行的实际 Agent 服务
3. **AgentServiceHandle**: Agent 服务的句柄，包含状态和通信通道
4. **ProjectWorkspace**: 项目工作空间管理

### 消息流程

```
HTTP Request → ProxyAgentManager → MPSC Channel → LocalSetAgentService → ACP Protocol
                                                                 ↓
HTTP Response ← ProxyAgentManager ← MPSC Channel ← LocalSetAgentService ← ACP Protocol
```

### Send trait 问题解决方案

- 使用 `tokio::task::LocalSet` 隔离非 Send 的 ACP 连接
- 通过 MPSC 通道实现跨线程通信
- 在 LocalSet 中运行实际的 ACP 协议处理

## 项目工作空间结构

```
project_workspace/
├── project_id_1/
│   ├── sessions/
│   └── workspace_files/
├── project_id_2/
│   ├── sessions/
│   └── workspace_files/
└── ...
```

## 配置选项

```rust
pub struct ProxyConfig {
    /// 项目工作空间根目录
    pub workspace_root: PathBuf,

    /// Agent服务空闲超时时间（秒）
    pub idle_timeout: Duration,

    /// 清理检查间隔（秒）
    pub cleanup_interval: Duration,

    /// 最大并发Agent服务数量
    pub max_concurrent_agents: usize,

    /// LocalSet运行时配置
    pub local_set_config: LocalSetConfig,
}
```

## 错误处理

系统提供完善的错误类型：

```rust
pub enum ProxyAgentError {
    AgentNotFound { project_id: String },
    AgentCreationFailed { source: Box<dyn std::error::Error + Send + Sync> },
    CommunicationError { message: String },
    LocalSetError { source: Box<dyn std::error::Error + Send + Sync> },
    WorkspaceError { path: PathBuf },
    ConfigError { message: String },
    InvalidProjectId { project_id: String },
    SessionError { message: String },
    TimeoutError { duration: Duration },
    ChannelSendError,
    ChannelReceiveError,
    IoError { source: std::io::Error },
}
```

## 性能考虑

- 使用 `Arc<DashMap>` 实现高性能的并发访问
- MPSC 通道确保消息的可靠传递
- 自动清理空闲 Agent 服务以释放资源
- 支持并发 Agent 服务数量限制

## 下一步

1. **完善 ACP 连接**: 实现实际的 ACP 协议连接和通信
2. **测试**: 编写单元测试和集成测试
3. **监控**: 添加服务监控和指标收集
4. **扩展**: 支持分布式部署和水平扩展