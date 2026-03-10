# RCoder - AI驱动的开发平台

> [English](README_EN.md)

RCoder 是一个基于 Rust 构建的现代化 AI 驱动开发平台，通过 **SACP (Symposium ACP)** 协议实现与多种 AI 代理的统一交互。项目采用**微服务架构**，支持 **Docker 容器化部署**和 **gRPC 高性能通信**。

## ✨ 核心特性

- 🔁 **反向代理**：集成 Cloudflare Pingora，高性能端口路由 `/proxy/{port}/{path}`
- 🌐 **HTTP API**：基于 Axum 的现代化 REST API 与统一 SSE 进度流
- 🤖 **多代理支持**：统一接入 Claude Code、Codex 等 AI 代理
- 🐳 **容器化架构**：每个项目对应独立的 Docker 容器，实现隔离和资源管理
- ⚡ **gRPC 通信**：基于 Tonic 的高性能内部通信，支持 Server Streaming
- 🔧 **配置系统**：命令行 > 环境变量 > 配置文件，多层配置优先级
- 📜 **API 文档**：自动化 API 文档（utoipa + Swagger UI）
- 📊 **可观测性**：Tracing + OpenTelemetry 完整链路追踪 + Pyroscope 性能分析
- 🖥️ **Computer Agent**：容器化 AI 代理环境，集成 VNC 远程桌面、音频流和 IME 输入

## 🏠 架构概览

### 整体架构

```
外部客户端 (HTTP/SSE)
    ↓
RCoder (HTTP API Server + Docker 管理 + gRPC 客户端)
    ↓ gRPC (Chat, CancelSession, SubscribeProgress)
Agent Runner (gRPC Server in Docker)
    ↓ Server Streaming (实时进度事件)
RCoder (转换为 SSE)
    ↓
外部客户端 (SSE)
```

### 核心组件

- **RCoder 主服务**：Axum HTTP 服务 + 容器管理 + gRPC 客户端
- **Agent Runner**：独立的 AI 代理运行环境（Docker 容器内），提供 gRPC 服务
- **Pingora 代理**：高性能反向代理服务，支持端口路由
- **Docker Manager**：全局容器生命周期管理

### 🛠️ 技术栈

| 组件类型 | 技术选型 | 说明 |
|----------|---------|------|
| **编程语言** | Rust 2024 Edition | 现代化系统编程语言 |
| **HTTP 框架** | Axum + Tower | 高性能异步 Web 框架 |
| **RPC 框架** | Tonic (gRPC) | 高性能 RPC 通信 |
| **AI 协议** | SACP + MCP | 支持多种 AI 代理协议 |
| **容器化** | Docker + Bollard | 容器管理和编排 |
| **数据库** | DuckDB + SQLx | 嵌入式分析数据库 |
| **日志系统** | Tracing + OpenTelemetry | 结构化日志和分布式追踪 |
| **性能分析** | Pyroscope | 持续性能剖析 |
| **命令行** | clap | 现代化命令行参数解析 |

## 🚀 快速开始

### 📝 环境要求

- Rust: 1.75+（2024 Edition）
- Docker（用于容器化部署）
- 可选：Claude Code CLI（用于 Claude 代理）

### 🛠️ 安装与运行

#### 本地开发

```bash
# 克隆仓库
git clone https://github.com/your-org/rcoder.git
cd rcoder

# 本地编译
cargo build --workspace

# 运行主服务
cargo run --bin rcoder

# 指定端口和项目目录
cargo run --bin rcoder -- --port 8087 --projects-dir ./my-projects
```

#### Docker 开发模式（推荐）

```bash
# 构建镜像并启动容器
make dev-build    # 构建 Docker 镜像
make dev-up       # 启动开发容器

# 代码修改后重启
make dev-restart  # 重新构建并重启

# 查看日志
make dev-logs

# 停止容器
make dev-down
```

#### 启用反向代理

```bash
# 启用 Pingora 反向代理
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080

# 指定默认后端端口
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080 --backend-port 3000
```

### 💻 命令行参数

| 参数 | 短参数 | 说明 | 示例 |
|------|--------|------|------|
| `--port` | `-p` | 设置主服务端口 | `--port 8087` |
| `--projects-dir` | `-d` | 设置项目工作目录 | `--projects-dir ./projects` |
| `--enable-proxy` | 无 | 启用 Pingora 反向代理 | `--enable-proxy` |
| `--proxy-port` | 无 | 设置 Pingora 监听端口 | `--proxy-port 8080` |
| `--backend-port` | 无 | 默认后端端口 | `--backend-port 3000` |

```bash
# 查看所有参数
cargo run --bin rcoder -- --help
```

## 📚 API 文档

### 🔁 Pingora 反向代理

Pingora 是项目内置的高性能反向代理。

```bash
# 启用代理
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080

# 代理请求示例（转发到 5173 端口）
curl "http://127.0.0.1:8080/proxy/5173/page/123/"
```

### 🏥 核心端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `/health` | GET | 健康检查 |
| `/chat` | POST | 发送聊天消息给 AI 代理 |
| `/agent/progress/{session_id}` | GET (SSE) | 获取实时进度流 |
| `/agent/session/cancel` | POST | 取消正在执行的任务 |
| `/agent/stop` | POST | 停止 Agent |
| `/agent/status/{project_id}` | GET | 查询 Agent 状态 |
| `/api/docs` | GET | Swagger UI API 文档 |

### gRPC 服务 (Agent Runner)

| 方法 | 类型 | 说明 |
|------|------|------|
| `Chat` | Unary | 发送聊天请求 |
| `SubscribeProgress` | Server Streaming | 订阅进度事件流 |
| `CancelSession` | Unary | 取消会话任务 |
| `GetStatus` | Unary | 查询 Agent 状态 |
| `StopAgent` | Unary | 停止 Agent |
| `GetContainerStatus` | Unary | 查询容器状态 |
| `GetVncStatus` | Unary | 查询 VNC 服务状态 |

### 🖥️ Computer Agent 端点

Computer Agent 提供容器化的 AI 代理环境，支持 VNC 远程桌面、音频流和 IME 输入。每个用户对应独立的 Docker 容器，多个项目可共享同一容器。

#### 核心接口

| 端点 | 方法 | 说明 |
|------|------|------|
| `/computer/chat` | POST | 发送聊天消息到 Computer Agent |
| `/computer/progress/{session_id}` | GET (SSE) | 获取实时进度流 |
| `/computer/agent/stop` | POST | 停止指定项目的 Agent（不销毁容器） |
| `/computer/agent/status` | POST | 查询 Agent 状态（alive/idle/busy） |
| `/computer/agent/session/cancel` | POST | 取消正在执行的会话 |

#### 桌面与媒体代理（通过 Pingora）

| 端点 | 方法 | 说明 |
|------|------|------|
| `/computer/desktop/{user_id}/{project_id}` | GET | 获取 VNC 桌面访问地址 |
| `/computer/vnc/{user_id}/{project_id}/{*path}` | GET | VNC/noVNC 代理（端口 6080） |
| `/computer/audio/{user_id}/{project_id}/{*path}` | GET | 音频流代理（端口 6089/6090） |
| `/computer/ime/{user_id}/{project_id}/{*path}` | GET | IME 输入法代理（端口 6091） |

#### Pod/容器管理

| 端点 | 方法 | 说明 |
|------|------|------|
| `/computer/pod/count` | GET | 容器数量统计（按服务类型分组） |
| `/computer/pod/list` | GET | 列出所有容器详情（支持分页 `?limit=100`） |
| `/computer/pod/ensure` | POST | 确保容器存在（幂等，不启动 Agent） |
| `/computer/pod/keepalive` | POST | 刷新容器活跃时间（防止自动清理） |
| `/computer/pod/restart` | POST | 重启容器（销毁并重建） |
| `/computer/pod/status` | GET | 查询容器状态（`?user_id=xxx`） |
| `/computer/pod/vnc-status` | GET | 查询 VNC 服务就绪状态 |

#### Computer Chat 请求示例

```bash
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user-123",
    "project_id": "my-project",
    "prompt": "帮我创建一个 Python Web 应用"
  }'
```

### 🚑 健康检查

```bash
curl -X GET http://localhost:8087/health
```

响应：
```json
{
  "status": "ok",
  "timestamp": "2024-01-01T00:00:00Z"
}
```

### 💬 聊天接口

```bash
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "你好，请帮我创建一个 Rust Web API 项目",
    "project_id": "my-project",
    "session_id": "optional-session-id"
  }'
```

### 📊 实时进度流

```bash
curl -X GET http://localhost:8087/agent/progress/your-session-id \
  -H "Accept: text/event-stream"
```

## 📁 项目结构

```
crates/
├── agent_abstraction/     # Agent 抽象层
├── agent_config/          # Agent 配置管理
├── agent_runner/          # Agent 运行时（gRPC 服务端）
│   ├── src/
│   │   ├── grpc/          # gRPC 服务实现
│   │   ├── proxy_agent/   # ACP 代理实现
│   │   └── service/       # 核心服务
│   └── Cargo.toml
├── docker_manager/        # Docker 容器管理
├── duckdb_manager/        # DuckDB 数据库管理
├── rcoder/               # 主应用
│   ├── src/
│   │   ├── grpc/         # gRPC 客户端
│   │   ├── handler/      # HTTP 处理器
│   │   ├── service/      # 业务服务
│   │   └── cleanup_task/ # 清理任务
│   └── Cargo.toml
├── rcoder-proxy/         # Pingora 代理封装
├── rcoder-telemetry/     # 遥测和追踪
└── shared_types/         # 共享类型和 Proto 定义
    └── proto/
        └── agent.proto   # gRPC 协议定义
```

## 配置

### 配置优先级

1. **命令行参数** - 最高优先级
2. **环境变量** - 中等优先级
3. **配置文件** - 较低优先级
4. **默认配置** - 最低优先级

### 配置文件 (config.yml)

```yaml
# 默认 Agent ID
default_agent_id: "claude-code-acp-ts"

# 项目工作目录
projects_dir: "./project_workspace"

# 主服务端口
port: 8087

# Docker 配置
docker_config:
  network_mode: "bridge"
  network_base_name: "agent-network"
  work_dir: "/app"
  auto_cleanup: true
  container_ttl_seconds: 3600
  api_timeout_seconds: 10
  cache_status_ttl_seconds: 10

# 容器清理配置
cleanup_config:
  enabled: true
  idle_timeout_seconds: 600
  cleanup_interval_seconds: 300
  container_protection_seconds: 300

# API Key 鉴权配置
api_key_auth:
  enabled: false
  api_key: "sk-xxx"

# 反向代理配置
proxy_config:
  listen_port: 8088
  default_backend_port: 8086
  backend_host: "127.0.0.1"
```

### 环境变量

| 变量名 | 说明 | 默认值 |
|--------|------|--------|
| `RCODER_PORT` | 服务端口 | 8087 |
| `RCODER_PROJECTS_DIR` | 项目目录 | ./project_workspace |
| `RCODER_NETWORK_MODE` | Docker 网络模式 | bridge |
| `RCODER_NETWORK_BASE_NAME` | 网络基础名称 | agent-network |
| `RCODER_API_TIMEOUT_SECONDS` | Docker API 超时 | 10 |
| `RCODER_API_KEY_ENABLED` | 启用 API Key 鉴权 | false |
| `RCODER_API_KEY` | API Key 密钥 | - |
| `RUST_LOG` | 日志级别 | info |

### 使用示例

```bash
# 使用环境变量
RCODER_PORT=8080 RUST_LOG=debug cargo run --bin rcoder

# 命令行参数优先级最高
RCODER_PORT=8080 cargo run --bin rcoder -- --port 9000
```

## 🔧 开发指南

### 运行测试

```bash
# 运行所有测试
cargo test --workspace

# 运行单元测试
make test-unit

# 运行集成测试
make test-integration
```

### 代码质量

```bash
# 代码格式化
cargo fmt

# 代码检查
cargo clippy

# 全面检查
cargo clippy -- -D warnings
```

### 本地开发

```bash
# 启动开发服务器
RUST_LOG=debug cargo run --bin rcoder -- --port 8087

# 监视文件变化
cargo install cargo-watch
cargo watch -x "run --bin rcoder"
```

## 🚀 部署指南

### Docker 部署

```bash
# 构建镜像
make docker-build

# 或分别构建
make docker-build-master       # 主服务镜像
make docker-build-agent-runner # Agent Runner 镜像

# 生产镜像（无调试工具）
make docker-build-agent-production
```

### Docker Compose

```bash
# 启动服务
make dev-up

# 查看状态
docker-compose -f docker/docker-compose.yml ps

# 停止服务
make dev-down
```

### Pyroscope 性能分析

```bash
# 启动 Pyroscope Server
make pyroscope-up

# 访问 Web UI
open http://localhost:4040

# 停止服务
make pyroscope-down
```

## 🐛 问题排查

### 常见问题

- **端口被占用**：使用 `--port` 参数指定其他端口
- **容器启动失败**：检查 Docker 服务状态和网络配置
- **gRPC 连接失败**：确认容器网络和端口配置
- **API Key 错误**：检查 `api_key_auth` 配置

### 调试模式

```bash
# 启用详细日志
RUST_LOG=debug cargo run --bin rcoder

# 查看容器日志
make dev-logs

# 进入容器调试
docker exec -it <container_id> /bin/bash
```

## 📈 更新日志

### v0.1.0 (当前版本)

#### 新增功能
- ✅ 基于 SACP 协议的 AI 代理统一管理
- ✅ gRPC 高性能通信架构
- ✅ Docker 容器化部署（项目级隔离）
- ✅ VNC/noVNC 远程桌面支持
- ✅ API Key 鉴权中间件
- ✅ 容器自动清理机制
- ✅ 多镜像配置支持
- ✅ Pyroscope 性能分析集成

#### 技术特性
- ✅ Rust 2024 Edition
- ✅ Tonic gRPC (v0.14.2)
- ✅ SACP 协议 (v10.1.0)
- ✅ MCP 协议支持 (rmcp v0.12.0)
- ✅ DuckDB 数据库
- ✅ OpenTelemetry 追踪
- ✅ eBPF 调试工具支持

## 🔗 相关链接

- **项目仓库**: [GitHub](https://github.com/your-org/rcoder)
- **问题追踪**: [Issues](https://github.com/your-org/rcoder/issues)
- **SACP 协议**: [Symposium ACP](https://crates.io/crates/sacp)
- **MCP 协议**: [rmcp](https://crates.io/crates/rmcp)

## 📝 许可证

本项目采用 MIT 或 Apache-2.0 双许可证。详见 [LICENSE](LICENSE) 文件。

## 🤝 贡献

欢迎贡献！请阅读 [CONTRIBUTING.md](CONTRIBUTING.md) 了解如何参与开发。

### 贡献指南

1. Fork 项目
2. 创建特性分支 (`git checkout -b feature/amazing-feature`)
3. 提交更改 (`git commit -m 'Add amazing feature'`)
4. 推送到分支 (`git push origin feature/amazing-feature`)
5. 开启 Pull Request

---

💫 **由 RCoder 团队精心打造，致力于推进 AI 驱动的现代化开发体验。**
