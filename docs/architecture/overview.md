# RCoder 架构总览

RCoder 是基于 Rust 构建的 AI 驱动开发平台，通过 **ACP（Agent Client Protocol）** 统一接入多种 AI 代理（Claude Code、Codex 等），并内置完整的 **UserApp 应用托管**能力。同一套代码支持 Docker 与 Kubernetes 双运行时。

## 主链路

请求主链是一条"HTTP 进、SSE 出"的转发链，中间段是 gRPC：

```
外部客户端 (HTTP/SSE)
    ↓ HTTP POST /chat 等
RCoder 主服务（Axum HTTP + 容器编排 + gRPC 客户端）
    ↓ gRPC（Tonic：Chat / SubscribeProgress / CancelSession …）
Agent Runner（容器/Pod 内的 gRPC 服务端）
    ↓ ACP 协议驱动 AI 代理（Claude Code / Codex …）
    ↑ Server Streaming（实时进度事件）
RCoder（gRPC 流转换为 SSE）
    ↓ SSE
外部客户端
```

- 外部只面对 RCoder 的 HTTP/SSE 接口，容器内的 gRPC 细节对外不可见（详见 [gRPC 内部通信](grpc.md)）。
- Agent Runner 是每个用户/项目对应的容器化代理运行环境；ACP agent 在容器内由 agent_runner 驱动，不在 RCoder 进程内。

## 核心组件

| 组件 | 职责 |
|------|------|
| **RCoder 主服务** | Axum HTTP 装配 + 容器编排 + gRPC 客户端 + file-server 内嵌 |
| **Agent Runner** | 容器内的 AI 代理运行环境，提供 gRPC 服务并驱动 ACP 连接 |
| **App Manager** | UserApp 全生命周期管理（dev 开发环境 / prod 运行环境） |
| **Pingora 代理** | 高性能反向代理（端口路由 + UserApp 应用代理） |
| **Docker Manager** | 容器生命周期管理（Docker / Kubernetes 双后端抽象） |

## 分层与 crate 地图

整个仓库是一个 Cargo workspace（`crates/app-cli` 除外——它是独立 workspace）。按层分组：

### 组合根与引擎

| crate | 职责 |
|-------|------|
| `rcoder` | 组合根：`run()` 装配 + 薄 bin |
| `rcoder-engine` | 引擎层：AppState 装配、gRPC 池、service 容器编排、userapp_builder/forward 编排、bootstrap、后台任务、file-server 内嵌与管理 |
| `http-server` | HTTP 面：axum handlers、router 装配、utoipa OpenAPI 文档、middleware |
| `desktop` | 桌面客户端骨架（gpui-kit 原生窗口 + 进程内完整 rcoder 服务） |

### Agent 域

| crate | 职责 |
|-------|------|
| `agent_runner` | Agent 运行时：容器内 gRPC 服务端 + ACP 连接驱动 |
| `agent_abstraction` | ACP Agent 抽象层：Claude Code / Codex 等统一接入 |
| `agent_config` | Agent 配置管理 |
| `agent_provisioning` | Agent 安装 / 供应 |
| `model_probe` | 模型预检探活（建会话前 fail-fast，模型不可用立即报错） |
| `rcoder-cli` | 本地测试 ACP agent 的 CLI 工具 |

### UserApp 域

| crate | 职责 |
|-------|------|
| `app_manager` | UserApp 生命周期 / 存储 / 文件管理（REST API） |
| `app-cli` | UserApp 构建 CLI（**独立 Cargo workspace**，npm 分发 `@nuwax-ai/app-cli`） |
| `workspace-manifest` | UserApp workspace 两级 manifest 类型 |
| `frontend-detector` | 前端项目框架探测（纯函数） |
| `preview-coordinator` | WebAgentRunner 开发阶段的 Vite 预览协调 |
| `runtime-state-layout` | 运行态状态根解析契约（app-cli 与平台同一解析规则） |

### 文件服务

| crate | 职责 |
|-------|------|
| `file-server` | 独立文件服务：文件/工作区/Git/技能（可嵌入 rcoder，也可 npm 独立分发 `@nuwax-ai/file-server`） |
| `file-server-userapp` | file-server 的 userapp 域实现 |
| `file-server-proxy` | file-server 分流代理 |

### 运行时与基础设施

| crate | 职责 |
|-------|------|
| `docker_manager` | 容器/运行时管理（Docker + K8s 双后端实现） |
| `container-runtime-api` | 容器运行时抽象 trait 层 |
| `rcoder-proxy` | Pingora 反向代理封装 |
| `rcoder-gateway` | K8s 无状态网关（header 注入 + Gateway API 路由） |
| `rcoder-storage` | 存储层（memory / PostgreSQL 后端） |
| `rcoder-telemetry` | 遥测（Tracing + OpenTelemetry） |

### 共享契约

| crate | 职责 |
|-------|------|
| `shared_types` | 跨 crate 业务契约与常量 |
| `shared_types_grpc` | gRPC proto 定义（`proto/agent.proto`） |
| `shared_types_i18n` | 错误码国际化 |
| `download_utils` / `process_utils` | 下载 / 进程工具 |

## 请求怎么走

**AI 会话（chat）**：客户端 POST `/chat` → rcoder 确保对应容器就绪 → 经连接池发 gRPC `Chat` 到 agent_runner → agent_runner 驱动 ACP agent → 进度经 `SubscribeProgress` Server Streaming 流回 → rcoder 转成 SSE 推给客户端。

**UserApp 操作**：客户端调 `/api/v1/userapp/*` REST 接口 → app_manager 受理生命周期操作 → 按 dev/prod 环境分派：dev 走 UserappBuilder 容器（构建/开发编排），prod 走容器运行时（Deployment + per-app PVC）→ 状态经 SSE 任务事件与 REST 查询暴露。业务概念详见 [UserApp 应用管理](../concepts/userapp.md)。

## 部署形态

同一引擎，三种形态：

| 形态 | 控制平面 | 适用 |
|------|---------|------|
| Docker Compose | 容器内 | 本地开发、单机 |
| Kubernetes | Pod（STS + PVC） | 生产、多租户/多副本 |
| deploy-host（宿主机） | 直接跑在宿主机，用本地 Docker/K8s 管动态容器 | 单机自用、桌面客户端基座，见[宿主机形态文档](../deployment/host.md) |

## 技术栈

| 组件类型 | 技术选型 | 说明 |
|----------|---------|------|
| 编程语言 | Rust 2024 Edition (1.85+) | MSRV 1.85 |
| HTTP 框架 | Axum 0.8 + Tower | 高性能异步 Web 框架 |
| RPC 框架 | Tonic 0.14 | gRPC 通信（全 rustls） |
| AI 协议 | agent-client-protocol v2 + MCP (rmcp) | ACP 官方 SDK，v1 wire 兼容 |
| 容器化 | Docker (Bollard) + Kubernetes (kube-rs) | 双运行时抽象 |
| 持久化 | PostgreSQL (SQLx, feature-gated) / 内存 | K8s 变体启用 PG，Docker 路径零依赖 |
| 日志系统 | Tracing + OpenTelemetry | 结构化日志（K8s 下写文件按天滚动） |
| 性能分析 | dial9 + hotpath | 事件级 Tokio tracing（本地 dev）+ 函数耗时剖析 |
| API 文档 | utoipa + Swagger UI + Scalar | 自动生成 OpenAPI |
