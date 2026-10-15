# 配置与 Agent 准备

本文说明主服务的配置入口、存储选择和 Agent 运行前置。部署命令分别见 [Docker Compose](deployment/docker.md)、[宿主机](deployment/host.md)和 [Kubernetes](deployment/kubernetes.md)。

## 配置文件与优先级

`RCODER_CONFIG_FILE` 指定主服务配置文件。普通运行默认使用当前目录的 `config.yml`；deploy-host 默认使用 `~/.rcoder/config.yml`，首次启动会创建缺失的默认配置。Compose 将 [docker/config.yml](../docker/config.yml) 挂载为容器内 `/app/config.yml`。

主服务对支持覆盖的配置按以下顺序取值：

1. 环境变量，例如 `RCODER_PORT`、`RCODER_PROJECTS_DIR`。
2. 命令行参数，例如 `--port`、`--projects-dir`。
3. 配置文件。
4. 内置默认值。

例如同时设置 `RCODER_PORT=8091` 与 `--port 8092`，最终使用 8091。并非每个 YAML 字段都有对应环境变量或 CLI 参数；文件路径由 `RCODER_CONFIG_FILE` 单独选择。完整字段结构可参考[默认配置模板](../crates/rcoder-engine/src/config/rcoder_default.yml)，Compose 的实际覆盖值以其配置与环境为准。

## 主服务命令行

```bash
cargo run -p rcoder --bin rcoder -- --help
```

| 参数 | 作用 |
|---|---|
| `--port` / `-p` | 主 HTTP 服务端口 |
| `--projects-dir` / `-d` | 项目工作区目录 |
| `--enable-proxy` / `-e` | 按 CLI 参数启用 Pingora 代理 |
| `--proxy-port` | 代理监听端口，与 `--enable-proxy` 一起使用 |
| `--backend-port` | 代理默认后端端口，与 `--enable-proxy` 一起使用 |

当前 `--projects-dir` 有 CLI 默认值 `./project_workspace`，未显式传参时也会覆盖配置文件中的项目目录；可通过显式 CLI 参数或优先级更高的 `RCODER_PROJECTS_DIR` 指定。`--proxy-port` 和 `--backend-port` 仅在 CLI 同时启用 `--enable-proxy` 时应用，不能单独用于覆盖已由 YAML 启用的代理。

普通主服务默认 HTTP 端口为 8087；Compose 默认宿主端口为 8090，Pingora 映射为 8089。宿主机部署的地址、端口和目录约定见[宿主机指南](deployment/host.md)。

## 两类存储分别配置

| 数据 | 配置入口 | Docker / 宿主机默认 | Kubernetes |
|---|---|---|---|
| Agent 项目、会话与容器关联 | `storage` / `RCODER_STORAGE_BACKEND` | `memory` | 可选 PostgreSQL；持久化和多副本部署使用 PostgreSQL |
| UserApp 生命周期、操作与恢复记录 | `userapp_storage` / `RCODER_USERAPP_STORAGE_BACKEND` | `auto` 解析为本地 Turso | `auto` 解析为 PostgreSQL，拒绝本地 Turso |

Agent 的 `memory` 不代表 UserApp 也使用内存。Agent PostgreSQL 后端需要 `rcoder-pg` 编译 feature；本地 UserApp Turso 需要默认启用的 `userapp-turso` feature。K8s 构建及连接前置见 [Kubernetes 指南](deployment/kubernetes.md)。

| 环境变量 | 用途 |
|---|---|
| `RCODER_STORAGE_BACKEND` | Agent 存储后端：`memory` / `postgres` |
| `RCODER_PG_URL` | Agent PostgreSQL 连接串；也支持 `RCODER_PG_HOST/PORT/USERNAME/PASSWORD/DATABASE` 分项配置 |
| `RCODER_USERAPP_STORAGE_BACKEND` | UserApp 控制存储：`auto` / `turso` / `postgres` |
| `RCODER_USERAPP_TURSO_PATH` | UserApp 本地数据库文件路径 |
| `RCODER_USERAPP_PG_URL` | UserApp PostgreSQL 连接串 |

普通本地默认 Turso 路径为 `data/rcoder/userapp.turso.db`。Compose 将数据库放在 `/app/data/userapp.turso.db`，`make dev-*` 使用的 named volume、备份和数据保留方式见 [Docker 指南](deployment/docker.md)。同一个本地数据目录只运行一个 RCoder 实例。

## Agent 与模型配置

会话使用的 Agent 与模型由请求中的 `agent_config`、`model_provider` 以及服务默认配置共同确定。`default_agent_id` 选择默认 Agent；旧配置字段 `default_agent` 仍是受支持的别名。Agent 的执行命令、参数和环境变量可参考[内置 Agent 配置](../crates/agent_config/configs/default_agents.json)与[自定义配置示例](../crates/agent_config/examples/agents.json)。

内置 Agent 配置使用 `{MODEL_PROVIDER_API_KEY}`、`{MODEL_PROVIDER_BASE_URL}`、`{MODEL_PROVIDER_DEFAULT_MODEL}` 等占位符，将请求中的模型配置传给实际 Agent。它们是 Agent 配置模板占位符，不能仅凭名称当作 RCoder 主服务的通用环境变量。完整请求字段以运行时 `/api/docs` 为准，调用链见[会话与 SSE](concepts/agent-sessions.md)。

容器模式下，Agent 命令及依赖必须在实际执行的 agent_runner 镜像内可用；只在宿主机安装不能替代容器内准备。要在本机单独测试 Claude Code 的 ACP 适配器，可按对应[示例配置](../crates/agent_config/examples/agents.json)准备：

```bash
npm install -g @anthropic-ai/claude-code
npm install -g @zed-industries/claude-code-acp
cargo run -p rcoder-cli -- --help
```

该适配器示例使用 `claude-code-acp`；内置默认配置使用 `claude-code-acp-ts`，两者的安装与命令配置应分别匹配。其他 Agent 也按所选命令和配置准备。真实 AI 请求还需要有效的模型凭据、地址和模型名称；健康检查成功只表示控制面可达。

## API 鉴权与敏感配置

`api_key_auth` 控制 RCoder 服务鉴权；`RCODER_API_KEY_ENABLED` 与 `RCODER_API_KEY` 可覆盖对应设置。启用后，请求携带 `x-api-key`，豁免路径以实际配置为准。首次创建配置会生成随机 API Key，但模板的 `api_key_auth.enabled` 默认为 `false`，deploy-host 也不会自动启用。生成了 key 不等于鉴权已生效，应检查实际配置并显式启用。

缺少或提供错误的 `x-api-key` 时，返回 HTTP 401，响应体的业务错误码为 `API_KEY_AUTH_FAILED`。此错误表示服务 API Key 鉴权失败，前端应提示检查 API Key 配置，不应按登录失效错误码 `4010` 跳转登录页。

```bash
curl http://127.0.0.1:8090/api/docs/openapi.json \
  -H "x-api-key: ${RCODER_API_KEY}"
```

服务 API Key 与模型提供商 API Key 分别用于控制面和模型调用。实际值通过本地环境或部署 Secret 注入；公开示例只使用占位符。配置文件、数据库、运行回执和日志可能包含敏感值，应保存在本地或受控存储。部署入口保护见 [SECURITY.md](../SECURITY.md)。

## 其他常用环境变量

| 变量 | 用途 |
|---|---|
| `DOCKER_SOCKET_PATH` | Docker socket 路径，常见值为 `/var/run/docker.sock` |
| `CONTAINER_RUNTIME` | 选择运行时；K8s 使用 `kubernetes` 并要求对应编译 feature |
| `RUST_LOG` | 日志过滤级别 |
| `RCODER_USERAPP_ENSURE_TIMEOUT_SECONDS` | UserApp 容器创建/就绪等待预算 |
| `RCODER_USERAPP_INTERACTIVE_ENSURE_WAIT_SECONDS` | 文件、Git 等交互请求定位 builder 的等待上限 |
| `RCODER_USERAPP_DEV_FORWARD_CONNECT_WAIT_SECONDS` | 转发请求发送前等待开发容器 TCP 可连接的预算 |

不同等待阶段各自有预算，调用方应结合原操作状态处理超时。错误与重试语义见[运行时错误契约](runtime-error-contract.md)。
