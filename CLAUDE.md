# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

RCoder 是一个基于 ACP (Agent Client Protocol) 的 AI 驱动开发平台，使用 Rust 构建。该项目集成了 Claude Code CLI，提供了 HTTP API 接口来管理 AI 代理和项目。

## 核心架构

### 工作空间结构
- **Workspace**: 使用 Cargo workspace 管理多个 crate
- **主要 crate**: `rcoder` (主应用), `claude` (Claude agent), `codex-acp-agent` (Codex agent), `acp_adapter` (ACP 适配器), `shared_types` (共享类型)

### AI 代理架构
项目支持三种 AI 代理类型：
1. **Codex**: 使用 ACP 协议和 MPMC 架构的代理
2. **Claude**: 使用 shell 命令调用的代理
3. **Proxy**: 使用 ACP 代理管理器的代理

### 核心组件
- **GlobalAgentManager**: 全局单例的 Codex 代理管理器 (MPMC 架构)
- **ProxyAgentManager**: ACP 代理管理器，处理代理生命周期
- **SessionManager**: ACP 会话管理器
- **AppState**: 应用状态管理，包含会话、进度事件等

## 开发命令

### 构建和运行
```bash
# 构建所有 crates
cargo build --release

# 运行主服务 (默认端口 3000)
cargo run --release

# 运行特定示例
cargo run --bin example_name
```

### 测试
```bash
# 运行所有测试
cargo test

# 运行特定 crate 的测试
cargo test -p rcoder
```

### 代码质量
```bash
# 格式化代码
cargo fmt

# 代码检查
cargo clippy

# 显示依赖关系图
cargo tree
```

## 重要技术细节

### ACP 协议集成
- 使用 `agent-client-protocol = "0.4"` 实现 ACP 协议
- AgentSideConnection 和 ClientSideConnection **未实现 Send trait**
- **必须**在 LocalSet 和 spawn_local 中使用这些连接

### 并发模型
- 使用 **DashMap** 替代 `Arc<RwLock<HashMap>>` 以获得更好的性能
- 主应用使用 `#[tokio::main(flavor = "current_thread")]`
- ACP 操作必须在 `LocalSet` 中执行以支持 `spawn_local`

### 会话管理
- 每个 HTTP 请求可以指定 session_id 或自动生成新的会话
- 会话信息存储在 DashMap 中，包含用户ID、项目ID、代理类型等
- 支持通过 SSE 流实时推送进度事件

### 项目管理
- 每个项目在 `./project_workspace/` 下创建独立目录
- 项目ID 使用 UUID 生成（去除中划线）
- 支持自动创建项目工作目录

## 环境配置

### 环境变量
- `PORT`: 服务端口 (默认: 3000)
- `DEFAULT_AGENT`: 默认代理类型 (codex/claude)
- `PROJECTS_DIR`: 项目工作目录 (默认: ./project_workspace)
- `RUST_LOG`: 日志级别

### 开发环境要求
- Rust 1.70+
- Claude Code CLI
- SQLite 3

## API 接口

### 核心端点
- `POST /chat`: 发送聊天消息 (默认使用 Codex)
- `POST /chat/proxy`: 通过 ACP 代理管理器发送消息
- `POST /chat/multipart`: 多媒体聊天 (文件上传)
- `POST /chat/acp-multipart`: ACP 原生内容块聊天
- `GET /progress/{session_id}`: SSE 进度流
- `GET /sessions/{session_id}`: 获取会话信息

### 响应格式
所有 API 响应都使用统一的 HttpResult 格式：
```rust
struct HttpResult<T> {
    success: bool,
    data: Option<T>,
    error: Option<ApiError>,
}
```

## 特殊注意事项

### 禁止事项
1. **禁止使用模拟响应逻辑** - 所有 AI 调用必须真实执行
2. **禁止编写 unsafe 代码** - 项目要求内存安全
3. **AgentSideConnection 必须在 LocalSet 中使用** - 由于未实现 Send trait

### 性能优化
- 使用 DashMap 进行并发访问
- 使用 MPMC 架构处理多个 AI 请求
- 使用 SSE 流进行实时进度更新

### 错误处理
- 使用 anyhow 进行错误传播
- 使用 HttpResult 统一 API 响应格式
- 详细的日志记录和追踪

## 调试和开发

### 日志配置
```bash
# 启用详细日志
RUST_LOG=debug cargo run

# 特定模块日志
RUST_LOG=rcoder=debug,tower_http=debug cargo run
```

### 测试数据
- `project_workspace/`: 项目工作目录
- `fixtures/`: 测试数据
- `examples/`: 示例代码

1. Always Response in 中文
2. 禁止使用模拟响应逻辑,比如为了简化逻辑,就直接使用模拟结果,是禁止的.
3. 禁止写 unsafe 代码
4. AgentSideConnection ,ClientSideConnection 没有实现 Send trait, 必须在 LocalSt and spawn_local 中使用
5. ACP协议的参考示例, 目录: /Volumes/soddy/git_workspace/rcoder/tmp/agent-client-protocol/rust/examples 下, agent.rs 是agent端的, client.rs 是client端的, 参考这两个文件来实现 ACP 协议.