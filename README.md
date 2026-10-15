# RCoder - AI 驱动的开发平台

> [English](README_EN.md)

RCoder 是基于 Rust 和 ACP（Agent Client Protocol）的 AI 开发平台。它通过 HTTP/SSE 接入 Claude Code、Codex 等 Agent，并管理 UserApp 从源码开发、构建到部署的生命周期。同一套代码支持 Docker 与 Kubernetes 运行时。

## 主要能力

- **Agent 会话**：统一 ACP 接入、gRPC 内部通信、SSE 实时进度与断线续传。
- **UserApp 托管**：开发/生产环境、构建发布、闲置回收、流量唤醒，以及保留数据的启停与恢复。
- **文件与开发服务**：工作区、Git、技能、开发服务器及应用日志接口。
- **Computer Agent**：容器化桌面，支持 VNC、音频和输入法透传。
- **双运行时**：本地 Docker 与 Kubernetes 容器编排，共享业务契约。
- **独立存储选择**：Agent 项目/会话支持内存或 PostgreSQL；UserApp 控制记录在本地使用 Turso，在 Kubernetes 使用 PostgreSQL。
- **API 与观测**：OpenAPI、Tracing、OpenTelemetry，以及按需启用的性能诊断。

## 架构

```text
客户端 HTTP/SSE
    ↓
RCoder：HTTP 接口 + 引擎编排 + 容器运行时
    ↓ Tonic gRPC
agent_runner：容器内运行环境
    ↓ ACP
AI Agent
```

HTTP 接口位于 `http-server`，业务编排位于 `rcoder-engine`，`rcoder` 负责应用装配。UserApp 由应用管理层、Docker/K8s 运行时及容器内 app-cli 管理。详见[架构总览](docs/architecture/overview.md)。

<a id="quick-start"></a>

## 快速开始

推荐使用本地 Docker Compose。准备 Docker、Compose 和 Make；源码构建工具链使用 Rust stable（2024 Edition），完整前置见[贡献指南](CONTRIBUTING.md#开发环境)。

```bash
git clone https://github.com/nuwax-ai/rcoder.git
cd rcoder
make dev-build
make dev-up
curl http://127.0.0.1:8090/health
```

启动后访问 [API 文档](http://127.0.0.1:8090/api/docs)。健康检查验证控制面；真实 AI 会话还需准备 Agent、模型地址与凭据，见[配置指南](docs/configuration.md)。

日常修改 Rust 后可用 `make dev-hot` 增量编译并重启；查看日志用 `make dev-logs`，停止用 `make dev-down`。镜像、持久化与完整操作步骤见 [Docker 指南](docs/deployment/docker.md)。

## 文档

完整目录见 [docs/README.md](docs/README.md)。

| 目的 | 指南 |
|---|---|
| 配置与模型准备 | [配置指南](docs/configuration.md) |
| 部署 | [Docker Compose](docs/deployment/docker.md)、[宿主机](docs/deployment/host.md)、[Kubernetes](docs/deployment/kubernetes.md) |
| 接入会话 | [Agent 与 SSE](docs/concepts/agent-sessions.md)、[权限审批](docs/concepts/permissions.md) |
| 使用应用与文件服务 | [UserApp](docs/concepts/userapp.md)、[文件服务](docs/concepts/file-services.md)、[Computer Agent](docs/concepts/computer-agent.md) |
| 构建与服务管理 | [app-cli](docs/development/app-cli.md)、[配置校验](docs/development/app-cli-validate.md) |
| 运行与排障 | [错误与重试](docs/runtime-error-contract.md)、[owner 恢复](docs/userapp-dev-owner-recovery.md)、[可观测性](docs/observability.md) |

完整 API 契约以运行时 OpenAPI 为准：`/api/docs` 提供 Swagger UI，`/api/docs/scalar` 提供 Scalar；包含主服务和 file-server 文档。

## 开发与验证

开发环境、构建、格式化、Clippy 和测试要求见 [CONTRIBUTING.md](CONTRIBUTING.md)。根 workspace 使用 `make test`；`crates/app-cli` 是独立 Cargo 项目，需要单独检查。

核心业务的真实 Compose 回归使用 `make test-e2e`，前置和验收边界见 [E2E 说明](tests-e2e/tools/README.md)。组件测试、部署验收和发布结果分别记录。

## 贡献与安全

欢迎通过 [Issues](https://github.com/nuwax-ai/rcoder/issues) 与 Pull Request 参与。提交前阅读[贡献指南](CONTRIBUTING.md)；漏洞请按 [SECURITY.md](SECURITY.md) 私下报告。

## 许可证

本项目采用 [Apache-2.0](LICENSE) 许可证。
