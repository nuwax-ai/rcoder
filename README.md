# RCoder - AI 驱动的开发平台

> [English](README_EN.md)

RCoder 是一个基于 Rust 构建的现代化 AI 驱动开发平台，通过 **ACP (Agent Client Protocol)** 协议统一接入多种 AI 代理（Claude Code、Codex 等）。项目采用**微服务架构**，支持 **Docker 与 Kubernetes 双运行时**、**gRPC 高性能通信**，并内置完整的 **UserApp 应用管理**（开发/生产双环境、发布链、自动回收）。

## ✨ 核心特性

- 🤖 **多代理支持**：基于官方 ACP SDK（`agent-client-protocol v2`，wire 兼容所有 ACP v1 agent）统一接入 Claude Code、Codex 等
- 🐳 **双运行时**：Docker Compose（本地开发）与 Kubernetes（生产，STS + per-agent PVC + CephFS）同一套代码
- 📦 **UserApp 应用管理**：dev/prod 双环境、构建发布链、文件/日志/存储管理、闲置回收与流量唤醒、彻底删除
- 🔁 **反向代理**：集成 Cloudflare Pingora，高性能端口路由与 UserApp 子路径代理
- 🌐 **HTTP API**：基于 Axum 的 REST API + 统一 SSE 进度流，utoipa 自动生成 OpenAPI 文档（Swagger UI + Scalar 双面）
- ⚡ **gRPC 通信**：基于 Tonic 的高性能内部通信，支持 Server Streaming 实时进度
- 🖥️ **Computer Agent**：容器化 AI 代理环境，集成 VNC 远程桌面、音频流和 IME 输入
- 🧩 **可插拔存储**：内存后端（默认）与 PostgreSQL 后端（K8s 变体）feature 切换
- 📊 **可观测性**：Tracing + OpenTelemetry 链路追踪 + Pyroscope 性能分析
- 🔒 **安全红线**：workspace 级 lint 禁止 `unsafe` 代码、禁止 `unwrap/expect` 进生产路径

## 🏠 架构概览

### 整体架构

```
外部客户端 (HTTP/SSE)
    ↓
RCoder (HTTP API Server + 容器管理 + gRPC 客户端)
    ↓ gRPC (Chat, CancelSession, SubscribeProgress)
Agent Runner (gRPC Server in 容器/Pod)
    ↓ Server Streaming (实时进度事件)
RCoder (转换为 SSE)
    ↓
外部客户端 (SSE)
```

### 核心组件

- **RCoder 主服务**：Axum HTTP 装配 + 容器编排 + gRPC 客户端 + file-server 嵌入
- **Agent Runner**：容器内的 AI 代理运行环境，提供 gRPC 服务并驱动 ACP 连接
- **App Manager**：UserApp 全生命周期（dev 开发环境 / prod 运行环境）
- **Pingora 代理**：高性能反向代理（端口路由 + UserApp 应用代理）
- **Docker Manager**：容器生命周期管理（Docker / K8s 双后端抽象）

### 🛠️ 技术栈

| 组件类型 | 技术选型 | 说明 |
|----------|---------|------|
| **编程语言** | Rust 2024 Edition (1.85+) | MSRV 1.85（edition 2024 最低要求） |
| **HTTP 框架** | Axum 0.8 + Tower | 高性能异步 Web 框架 |
| **RPC 框架** | Tonic 0.14 | 高性能 gRPC 通信（全 rustls） |
| **AI 协议** | agent-client-protocol v2 + MCP (rmcp) | ACP 官方 SDK，v1 wire 兼容 |
| **容器化** | Docker (Bollard) + Kubernetes (kube-rs) | 双运行时抽象 |
| **持久化** | PostgreSQL (SQLx, feature-gated) / 内存 | K8s 变体启用 PG，Docker 路径零依赖 |
| **日志系统** | Tracing + OpenTelemetry | 结构化日志（K8s 下写文件按天滚动） |
| **性能分析** | Pyroscope | 持续性能剖析 |
| **API 文档** | utoipa + Swagger UI + Scalar | 自动生成 OpenAPI |

## 🚀 快速开始

### 📝 环境要求

- Rust: 1.85+（2024 Edition）
- Docker（本地 Docker Compose 模式）或 K8s 集群（devspace，推荐 OrbStack）
- 可选：Claude Code CLI（用于 Claude 代理）

### 🛠️ 安装与运行

#### 本地开发

```bash
# 克隆仓库
git clone https://github.com/nuwax-ai/rcoder.git
cd rcoder

# 本地编译（workspace 全量）
cargo build --workspace

# 运行主服务（默认端口 8087）
cargo run --bin rcoder

# 指定端口和项目目录
cargo run --bin rcoder -- --port 8087 --projects-dir ./my-projects
```

#### Docker Compose 开发模式（推荐，端口 8090）

```bash
# 首次部署：构建镜像 + 启动容器
make dev-build    # 构建 Docker 镜像
make dev-up       # 启动容器（主服务 8090 / Pingora 8089）

# 日常开发：改 Rust 源码后秒级热编译（容器内增量编译 + 替换二进制 + 重启）
make dev-hot      # 推荐！增量 <2min，无需全量重建

# 全量重建（仅改 Dockerfile / Cargo.toml 依赖 / 非 Rust 文件时需要，10+ 分钟）
make dev-restart

# 日志 / 停止
make dev-logs
make dev-down
```

#### 本地 K8s 模式（devspace + OrbStack，端口 8290）

```bash
# 初始化并启动（namespace: rcoder-dev）
make devspace-init
make devspace-dev

# 健康检查
curl http://127.0.0.1:8290/health
```

> ⚠️ K8s/devspace 模式下 rcoder 日志**写文件不写 stdout**（`/app/logs/rcoder.YYYY-MM-DD`，JSON 格式），`kubectl logs` 几乎为空属正常现象。

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
| `--enable-proxy` | `-e` | 启用 Pingora 反向代理 | `--enable-proxy` |
| `--proxy-port` | 无 | 设置 Pingora 监听端口 | `--proxy-port 8080` |
| `--backend-port` | 无 | 默认后端端口 | `--backend-port 3000` |

```bash
# 查看所有参数
cargo run --bin rcoder -- --help
```

## 📚 API 文档

启动后访问 Swagger UI（`/api/docs`，主文档 + file-server 双面）或 Scalar 文档查看全部接口。以下为代表性端点。

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

### 📦 UserApp 应用管理（`/api/v1/userapp/*`）

UserApp 是面向用户的应用托管面，每个 app 具备 **dev 开发环境**（UserappBuilder 容器，常驻自愈）与 **prod 运行环境**（Deployment + per-app PVC）：

| 端点（节选） | 说明 |
|------|------|
| `POST /{app_id}/start` | 部署/启动应用（url 部署自动创建） |
| `POST /{app_id}/stop` / `restart` | 停止（scale-to-zero，支持流量唤醒）/ 重启 |
| `POST /{app_id}/{app_stage}/delete` | 删除 prod 运行容器（默认保留存储，purge=true 连数据面） |
| `POST /{app_id}/delete/app` | **彻底删除**：dev+prod 容器、两侧 PVC 与元数据一步收敛（幂等） |
| `GET /{app_id}/{app_stage}/storage` 等存储族 | 存储查询/清空/销毁 |
| `POST /{app_id}/{app_stage}/upload` 等文件族 | 文件上传/列表/删除 |
| `GET /proxy/app/{stage}/{user_id}/{app_id}/{*path}` | Pingora 应用访问代理 |

配套工具链：`app-cli`（应用构建 CLI，npm 分发 `@nuwax-ai/app-cli`）、`file-server`（文件/构建服务，npm 分发 `@nuwax-ai/file-server`）。闲置应用自动回收（scale-to-zero），可配置流量自动唤醒。

### 🖥️ Computer Agent 端点

Computer Agent 提供容器化的 AI 代理环境，支持 VNC 远程桌面、音频流和 IME 输入。每个用户对应独立的容器/Pod，多个项目可共享。

| 端点 | 方法 | 说明 |
|------|------|------|
| `/computer/chat` | POST | 发送聊天消息到 Computer Agent |
| `/computer/progress/{session_id}` | GET (SSE) | 获取实时进度流 |
| `/computer/agent/stop` | POST | 停止指定项目的 Agent（不销毁容器） |
| `/computer/agent/status` | POST | 查询 Agent 状态（alive/idle/busy） |
| `/computer/agent/session/cancel` | POST | 取消正在执行的会话 |
| `/computer/pod/ensure` | POST | 确保容器/Pod 存在（幂等） |
| `/computer/pod/list` / `count` / `restart` | GET/POST | 容器管理 |
| `/computer/vnc/{user_id}/{project_id}/{*path}` | GET | VNC/noVNC 桌面代理 |
| `/computer/audio/...`、`/computer/ime/...` | GET | 音频流 / 输入法代理 |

### gRPC 服务（Agent Runner，proto 见 `crates/shared_types_grpc/proto/agent.proto`）

| 方法 | 类型 | 说明 |
|------|------|------|
| `Chat` | Unary | 发送聊天请求 |
| `SubscribeProgress` | Server Streaming | 订阅进度事件流 |
| `CancelSession` | Unary | 取消会话任务 |
| `ResolvePermission` | Unary | 权限请求裁决 |
| `GetStatus` | Unary | 查询 Agent 状态 |
| `StopAgent` | Unary | 停止 Agent |
| `GetContainerStatus` / `GetVncStatus` | Unary | 容器 / VNC 状态 |
| `ListAgents` / `GetAgent` / `CheckAgent` | Unary | Agent 清单与探测 |
| `InstallAgent` / `UninstallAgent` | Streaming/Unary | Agent 安装/卸载 |

### 💬 使用示例

```bash
# 健康检查
curl http://localhost:8087/health

# 聊天
curl -X POST http://localhost:8087/chat \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "你好，请帮我创建一个 Rust Web API 项目",
    "project_id": "my-project"
  }'

# 实时进度流
curl http://localhost:8087/agent/progress/your-session-id \
  -H "Accept: text/event-stream"
```

## 📁 项目结构

```
crates/
├── rcoder/                  # 主应用（HTTP 装配 + 容器编排 + gRPC 客户端）
├── agent_runner/            # Agent 运行时（容器内 gRPC 服务端 + ACP 连接驱动）
├── agent_abstraction/       # ACP Agent 抽象层（Claude Code / Codex 统一接入）
├── agent_config/            # Agent 配置管理
├── agent_provisioning/      # Agent 安装/供应
├── model_probe/             # 模型预检探活（建会话前 fail-fast）
├── docker_manager/          # 容器/运行时管理（Docker + K8s 双后端实现）
├── container-runtime-api/   # 容器运行时抽象 trait 层
├── app_manager/             # UserApp 生命周期/存储/文件管理
├── file-server/             # 独立文件服务（可嵌入 rcoder，也可 npm 独立分发）
├── file-server-userapp/     # file-server 的 userapp 域实现
├── file-server-proxy/       # file-server 分流代理
├── rcoder-proxy/            # Pingora 反向代理封装
├── rcoder-storage/          # 存储层（memory / PostgreSQL 后端）
├── rcoder-gateway/          # K8s 无状态网关（header 注入 + Envoy Gateway 路由）
├── rcoder-telemetry/        # 遥测（Tracing + OTel + Pyroscope）
├── rcoder-cli/              # 本地测试 ACP agent 的 CLI 工具
├── shared_types/            # 共享类型与常量
├── shared_types_grpc/       # gRPC proto 定义（proto/agent.proto）
├── shared_types_i18n/       # 错误码国际化
├── workspace-manifest/      # UserApp workspace 两级 manifest 类型
├── frontend-detector/       # 前端项目框架探测（纯函数）
├── download_utils/          # 下载工具
├── process_utils/           # 进程工具
├── app-cli/                 # UserApp 构建 CLI（独立 workspace，npm 分发）
tests-e2e/                   # e2e 测试（Docker Compose 真实环境）
```

## ⚙️ 配置

### 配置优先级

1. **命令行参数** - 最高优先级
2. **环境变量** - 中等优先级
3. **配置文件**（config.yml，自动生成/挂载）- 较低优先级
4. **默认配置** - 最低优先级

### 配置文件示例（节选，完整见 [docker/config.yml](docker/config.yml)）

```yaml
# 默认使用的 AI 代理类型 (Claude/Codex)
default_agent: "Claude"

# 项目工作目录
projects_dir: "/app/project_workspace"

# 主服务端口（会被环境变量 RCODER_PORT 覆盖）
port: 8090

# 容器清理配置
cleanup_config:
  enabled: true
  idle_timeout_seconds: 600
  cleanup_interval_seconds: 500
  docker_stop_timeout_seconds: 30
  container_protection_seconds: 300

# Pingora 反向代理配置
proxy_config:
  listen_port: 8088
  default_backend_port: 8090
  backend_host: "127.0.0.1"

# Docker 配置（多镜像 + 按服务类型资源限额，K8s 模式下作为 Pod resources 来源）
docker_config:
  multi_image_config:
    # 全局默认 / 按架构（arm64/amd64）/ 按服务类型的镜像配置
    ...

# 60000 file-server 分流代理（dev 形态）
file_server_proxy:
  listen_port: 60000
  rust_upstream_port: 8090
  ts_upstream_port: 60001
  policy: all_rust
```

### 环境变量（常用）

| 变量名 | 说明 | 默认值 |
|--------|------|--------|
| `RCODER_PORT` | 服务端口 | 8087 |
| `RCODER_PROJECTS_DIR` | 项目目录 | ./project_workspace |
| `RCODER_STORAGE_BACKEND` | 存储后端（memory/postgres） | memory |
| `DOCKER_SOCKET_PATH` | Docker socket 路径 | /var/run/docker.sock |
| `RCODER_DOCKER_IMAGE` 等 | 自定义镜像配置 | - |
| `RUST_LOG` | 日志级别 | info |

## 🔧 开发指南

### 运行测试

```bash
# 全量测试（CI 同款门禁；e2e 需要 Docker 环境）
cargo test --workspace --all-features    # = make test-all

# 单 crate
cargo test -p app_manager --all-features

# 仅单元/集成
make test-unit
make test-integration
```

### 代码质量

```bash
# 格式化
cargo fmt

# Lint（workspace 当前零告警，含 -D unsafe_code / await_holding_lock 等红线）
cargo clippy --workspace --all-features --tests
```

### 日常开发节奏

```bash
# Docker Compose 模式：改码 → 热编译秒级生效
make dev-hot

# K8s/devspace 模式：源码自动 sync，但需重启 rcoder 进程生效
# （容器内 /app/dev-rcoder.sh restart，增量编译 ~1min）

# 本地直跑
RUST_LOG=debug cargo run --bin rcoder -- --port 8087
```

## 🚀 部署指南

### Docker 镜像

```bash
make docker-build                 # 全量
make docker-build-master          # 主服务镜像
make docker-build-agent-runner    # Agent Runner 镜像
make docker-build-agent-production # 生产镜像（无调试工具）
make docker-build-app-runtime     # UserApp 运行时镜像（统一 5 语言）
```

### Kubernetes

K8s 模式核心概念：

- **Agent Runner**：StatefulSet（STS）+ per-agent PVC（**停止不删 PVC**，数据复用下次重建挂回；OOM 自动容器级重启自愈）
- **UserApp prod**：Deployment + per-app PVC（删除默认保留存储，销毁走显式接口）
- **dev 开发环境**：UserappBuilder STS，空闲自动回收
- 本地集群用 devspace（Envoy Gateway），生产用 Cilium 网关

```bash
# 本地 K8s 开发
make devspace-dev

# 生产部署（Helm，配置见独立部署仓库）
```

### Pyroscope 性能分析

```bash
make pyroscope-up     # 启动，访问 http://localhost:4040
make pyroscope-down
```

## 🐛 问题排查

### 常见问题

- **端口被占用**：使用 `--port` 参数指定其他端口
- **容器启动失败**：检查 Docker 服务状态和网络配置
- **gRPC 连接失败**：确认容器网络和端口配置（K8s 下为 `{pod}-svc.{ns}.svc.cluster.local:50051`）
- **K8s 下 kubectl logs 为空**：正常现象，rcoder 日志写 pod 内 `/app/logs/rcoder.YYYY-MM-DD`（JSON 按天滚动）
- **API Key 错误**：检查 `api_key_auth` 配置

### 调试模式

```bash
# 启用详细日志
RUST_LOG=debug cargo run --bin rcoder

# 查看容器日志
make dev-logs

# 进入容器调试
docker exec -it <container_id> /bin/bash

# 本地测试 ACP agent（无需起完整服务）
cargo run -p rcoder-cli
```

## 📈 版本与主要能力

当前 workspace 版本 **0.1.3**。主要能力：

- ✅ ACP 官方 SDK v2（wire 兼容 Claude Code / Codex 等 ACP v1 agent）
- ✅ gRPC 通信架构（Tonic 0.14，Server Streaming 进度流）
- ✅ Docker + Kubernetes 双运行时（STS/PVC 生命周期、OOM 自愈）
- ✅ UserApp 应用管理（dev/prod 双环境、发布链、回收/唤醒、彻底删除）
- ✅ Computer Agent（VNC/noVNC、音频流、IME）
- ✅ 模型预检 fail-fast（model_probe）
- ✅ API Key 鉴权中间件
- ✅ OpenTelemetry 追踪 + Pyroscope 性能分析
- ✅ file-server / app-cli npm 独立分发

## 🔗 相关链接

- **项目仓库**: [github.com/nuwax-ai/rcoder](https://github.com/nuwax-ai/rcoder)
- **问题追踪**: [Issues](https://github.com/nuwax-ai/rcoder/issues)
- **ACP 协议**: [agent-client-protocol](https://crates.io/crates/agent-client-protocol)
- **MCP 协议**: [rmcp](https://crates.io/crates/rmcp)

## 📝 许可证

本项目采用 Apache-2.0 许可证。详见 [LICENSE](LICENSE) 文件。

## 🤝 贡献

1. Fork 项目
2. 创建特性分支 (`git checkout -b feature/amazing-feature`)
3. 提交更改（遵循 Conventional Commits，中文描述）
4. 推送到分支 (`git push origin feature/amazing-feature`)
5. 开启 Pull Request

---

💫 **由 RCoder 团队精心打造，致力于推进 AI 驱动的现代化开发体验。**
