# RCoder - AI驱动的开发平台

RCoder 是一个基于 Rust 构建的现代化 AI 驱动开发平台，通过 ACP (Agent Client Protocol) 协议实现与多种 AI 代理的统一交互。平台提供简洁的 HTTP API 接口，让开发者能够轻松集成和管理 AI 辅助开发功能。

## ✨ 核心特性

- 🔁 反向代理：集成 Cloudflare Pingora，高性能端口路由 `/proxy/{port}/{path}`
- 🌐 HTTP API：基于 Axum 的现代化 REST API 与统一 SSE 进度流
- 🤖 多代理支持：统一接入 Codex、Claude Code 等 AI 代理
- ⚡ 异步架构：Tokio 驱动的高并发异步处理
- 🔧 配置系统：命令行 > 环境变量 > 配置文件，多层配置优先级
- 📜 文档与可视化：自动化 API 文档（utoipa + Swagger UI）
- 📊 可观测性：Tracing + OpenTelemetry 完整链路追踪


## 🏠 架构概览

```mermaid
graph TB
    A[Client] --> B[Axum HTTP Server]
    A --> C[Pingora Proxy]
    B --> D[API Routes]
    B --> E[Agent Worker (LocalSet)]
    C --> F[Backends: 127.0.0.1:{port}]
```

- Axum 主服务负责业务 API、会话管理与 SSE 进度流
- Pingora 独立监听代理端口，按路径前缀 `/proxy/{port}/{path}` 转发到指定后端
- 两者并行运行，互不阻塞；Axum 中的 `/proxy/...` 路由仅作文档与重定向到 Pingora

### 🛠️ 技术栈

| 组件类型 | 技术选型 | 说明 |
|----------|---------|------|
| **编程语言** | Rust 2024 Edition | 现代化系统编程语言 |
| **HTTP 框架** | Axum + Tower | 高性能异步 Web 框架 |
| **异步运行时** | Tokio | Rust 生态最成熟的异步运行时 |
| **AI 协议** | ACP (Agent Client Protocol) | 统一的 AI 代理通信协议 |
| **序列化** | Serde + JSON/YAML | 数据序列化和配置管理 |
| **日志系统** | Tracing + OpenTelemetry | 结构化日志和分布式追踪 |
| **命令行** | clap | 现代化命令行参数解析 |
| **文档** | utoipa + Swagger UI | 自动 API 文档生成 |

## 🚀 快速开始

### 📝 环境要求

- Rust: 1.75+（2024 Edition）
- 可选：Claude Code CLI（用于 Claude 代理）
- 可选：OpenAI Codex（用于 Codex 代理）

### 🛠️ 安装与运行

1) 克隆并构建
```bash
git clone https://github.com/your-org/rcoder.git
cd rcoder
cargo build --workspace
```

2) 运行主服务（Axum）
```bash
# 使用默认端口 3000
cargo run --bin rcoder

# 指定端口和项目目录
cargo run --bin rcoder -- --port 8087 --projects-dir ./my-projects
```

3) 启用并运行 Pingora 反向代理
```bash
# 在同一进程中启用 Pingora，监听 8080
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080

# 指定默认后端端口（当请求未指定端口时使用）
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080 --default-backend-port 3000
```

4) 代理请求示例
```bash
# 将请求发送到 Pingora 监听端口（如 8080），路径前缀指定目标端口和路径
curl "http://127.0.0.1:8080/proxy/5173/page/1977625137029189632/prod/"
```

> 提示：若请求误发到主服务端口（如 3000/8087）上的 `/proxy/...` 路由，将收到 307 重定向到 Pingora 端口；建议直接请求 Pingora 端口。


### 💻 命令行参数

| 参数 | 短参数 | 说明 | 示例 |
|------|--------|------|------|
| `--port` | `-p` | 设置主服务端口（Axum） | `--port 8087` |
| `--projects-dir` | `-d` | 设置项目工作目录 | `--projects-dir ./projects` |
| `--enable-proxy` | 无 | 启用 Pingora 反向代理 | `--enable-proxy` |
| `--proxy-port` | 无 | 设置 Pingora 监听端口 | `--proxy-port 8080` |
| `--default-backend-port` | 无 | 未指定端口时的默认后端端口 | `--default-backend-port 3000` |

```bash
# 查看所有参数
cargo run --bin rcoder -- --help

# 启动主服务 + 启用代理
cargo run --bin rcoder -- --port 8087 --enable-proxy --proxy-port 8080
```


### 🌍 AI 代理配置

#### Claude Code Agent

需要安装 Claude Code CLI：

```bash
# 使用 npm 安装
npm install -g @anthropic-ai/claude-code

# 或者使用 pip 安装
pip install claude-dev
```

配置环境变量：
```bash
export ANTHROPIC_API_KEY="your-api-key"
export ANTHROPIC_MODEL="claude-3-sonnet-20240229"
```

#### OpenAI Codex Agent

需要配置 Codex 相关设置，参考 [Codex 文档](https://github.com/openai/codex)。

## 📚 API 文档

### 🔁 Pingora 反向代理使用指南

Pingora 是项目内置的高性能反向代理，所有真实的代理请求必须发送到 Pingora 的监听端口，并使用路径前缀形式 `/proxy/{port}/{path}` 来指定目标后端端口与路径。

#### 1. 启用与启动

- 在命令行启用代理：

```bash
# 启用代理并指定代理监听端口（例如 8080）
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080

# 可选：指定默认后端端口（当未在路径中指定时使用）
cargo run --bin rcoder -- --enable-proxy --proxy-port 8080 --default-backend-port 3000
```

- 或在配置文件 `config.yml` 中设置：

```yaml
proxy_config:
  listen_port: 8080           # Pingora 监听端口
  default_backend_port: 3000  # 默认后端端口
  backend_host: "127.0.0.1"    # 后端主机地址
  port_param: "port"            # 字段保留（Pingora模式下不解析查询参数）
```

启动成功后日志中会看到：

```
启动 Pingora 反向代理服务，监听端口: 8080
📡 监听端口: 8080
🔄 路由规则: /proxy/{port}{/path}
```

#### 2. 请求格式

- 仅支持路径前缀形式（Pingora 模式）：
  - `GET http://localhost:<listen_port>/proxy/{port}/{path}`
  - 例如：`http://localhost:8080/proxy/5173/page/1977625137029189632/prod/`

- 注意：Pingora 模式下不支持 `?port=...` 查询参数提取端口（遵循项目规范“代理端口提取规则”）。

#### 3. 示例

假设本机已有一个运行在 5173 端口的前端开发服务器（如 Vite）：

```bash
# 将请求发送到 Pingora 监听端口（如 8080），并使用路径前缀指定目标端口和路径
curl "http://127.0.0.1:8080/proxy/5173/page/1977625137029189632/prod/"
```

如果你误把请求发到主服务端口（如 3000/8087）的 Axum 路由 `/proxy/...`，服务会返回 307 重定向到 Pingora 的端口。你可以使用：

```bash
# 自动跟随重定向到 Pingora 端口
curl --location "http://127.0.0.1:3000/proxy/5173/page/1977625137029189632/prod/"
```

推荐直接请求 Pingora 端口，避免不必要的重定向跳转。

#### 4. 常见问题排查

- “Failed to connect to localhost port 5173”：请确认目标后端（5173）确实在本机启动，Pingora 只能代理到存在的服务。
- 收到 JSON 演示响应而非真实转发：说明命中了 Axum 的文档接口或重定向前的端口，请改为请求 Pingora 的 `listen_port`。
- 端口提取失败：检查路径是否正确为 `/proxy/{port}/{path}`，以及 `{port}` 是否为有效的数字端口。

#### 5. 相关接口（文档与状态）

- `GET /proxy/status`：查看代理服务状态（在主服务端口）
- `GET /proxy/config`：查看代理配置（在主服务端口）
- `GET /proxy/stats`：查看代理统计信息（在主服务端口）

> 说明：以上接口用于状态查询与文档展示；真实代理请求请直接发送到 Pingora 的监听端口。



### 🏥 核心端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `/health` | GET | 健康检查 |
| `/chat` | POST | 发送聊天消息给 AI 代理 |
| `/agent/progress/{session_id}` | GET (SSE) | 获取统一实时进度流 |
| `/agent/session/cancel` | POST | 取消正在执行的任务 |
| `/agent/stop` | POST | 停止当前 Agent |
| `/agent/status/{project_id}` | GET | 查询 Agent 状态 |
| `/api/docs` | GET | Swagger UI API 文档 |

Pingora 相关（主服务端口，仅文档/状态）：
- `GET /proxy/status`、`GET /proxy/config`、`GET /proxy/stats`
- `GET /proxy/{port}/{path}`（返回 307 重定向至 Pingora 端口）

真实代理请求需直接发送到 Pingora 监听端口：
- `http://localhost:{listen_port}/proxy/{port}/{path}`


### 🚑 健康检查

```bash
curl -X GET http://localhost:3000/health
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
curl -X POST http://localhost:3000/chat \
  -H "Content-Type: application/json" \
  -d '{
    "message": "你好，请帮我创建一个 Rust Web API 项目",
    "session_id": "optional-session-id"
  }'
```

### 📊 实时进度流

```bash
curl -X GET http://localhost:3000/agent/progress/your-session-id \
  -H "Accept: text/event-stream"
```

SSE 事件格式：
```
data: {"type": "progress", "content": "正在处理您的请求..."}

data: {"type": "result", "content": "项目创建完成"}
```

## 📁 项目结构

```
.
├── crates
│   ├── acp_adapter
│   │   ├── src
│   │   │   ├── lib.rs
│   │   │   ├── mention.rs
│   │   │   └── types.rs
│   │   └── Cargo.toml
│   ├── claude-code-agent
│   │   ├── src
│   │   │   ├── lib.rs
│   │   │   ├── main.rs
│   │   │   └── util.rs
│   │   └── Cargo.toml
│   ├── codex-acp-agent
│   │   ├── src
│   │   │   ├── commands
│   │   │   │   ├── commands.rs
│   │   │   │   └── mod.rs
│   │   │   ├── fs
│   │   │   │   ├── bridge.rs
│   │   │   │   ├── mcp_server.rs
│   │   │   │   └── mod.rs
│   │   │   ├── agent.rs
│   │   │   ├── lib.rs
│   │   │   └── main.rs
│   │   └── Cargo.toml
│   ├── nuwax_parser
│   │   ├── src
│   │   │   ├── model
│   │   │   │   ├── mod.rs
│   │   │   │   ├── source_code.rs
│   │   │   │   └── tests.rs
│   │   │   ├── project_op
│   │   │   │   ├── mod.rs
│   │   │   │   ├── project_read.rs
│   │   │   │   └── project_zip.rs
│   │   │   └── lib.rs
│   │   ├── Cargo.toml
│   │   └── README.md
│   ├── pingora-proxy
│   │   ├── src
│   │   │   ├── config.rs
│   │   │   ├── lib.rs
│   │   │   ├── pingora_server.rs
│   │   │   ├── server.rs
│   │   │   ├── service.rs
│   │   │   └── tests.rs
│   │   └── Cargo.toml
│   ├── rcoder
│   │   ├── examples
│   │   │   └── multimedia_chat_example.rs
│   │   ├── src
│   │   │   ├── handler
│   │   │   │   ├── agent_cancel_handler.rs
│   │   │   │   ├── agent_session_notification.rs
│   │   │   │   ├── agent_stop_handler.rs
│   │   │   │   ├── chat_handler.rs
│   │   │   │   ├── health_handler.rs
│   │   │   │   ├── mod.rs
│   │   │   │   ├── proxy_api.rs
│   │   │   │   └── proxy_handler_api.rs
│   │   │   ├── middleware
│   │   │   │   ├── mod.rs
│   │   │   │   └── tracing_middleware.rs
│   │   │   ├── model
│   │   │   │   ├── agent_model.rs
│   │   │   │   ├── agent_session_notify.rs
│   │   │   │   ├── app_error.rs
│   │   │   │   ├── attachment.rs
│   │   │   │   ├── chat_prompt.rs
│   │   │   │   ├── http_result.rs
│   │   │   │   └── mod.rs
│   │   │   ├── proxy_agent
│   │   │   │   ├── acp_agent.rs
│   │   │   │   ├── agent_service.rs
│   │   │   │   ├── agent_stop_handle.rs
│   │   │   │   ├── channel_utils.rs
│   │   │   │   ├── claude_code_agent.rs
│   │   │   │   ├── cleanup_task.rs
│   │   │   │   ├── codex_agent.rs
│   │   │   │   └── mod.rs
│   │   │   ├── service
│   │   │   │   ├── mod.rs
│   │   │   │   └── session_cache.rs
│   │   │   ├── utils
│   │   │   │   ├── content_builder.rs
│   │   │   │   ├── file_utils.rs
│   │   │   │   ├── mcp_config.rs
│   │   │   │   ├── mod.rs
│   │   │   │   └── system_prompt.rs
│   │   │   ├── config.rs
│   │   │   ├── lib.rs
│   │   │   ├── main.rs
│   │   │   └── router.rs
│   │   └── Cargo.toml
│   └── shared_types
│       ├── src
│       │   ├── model
│       │   │   ├── mod.rs
│       │   │   └── model_provider.rs
│       │   └── lib.rs
│       └── Cargo.toml
├── Cargo.toml
├── config.yml
├── README.md
└── ...
```

- `crates/rcoder`: 主应用（Axum 路由、业务、配置、代理启动）
- `crates/pingora-proxy`: Pingora 代理封装（配置、服务、服务器管理）
- 其他 crates：协议适配、代理实现、工具与共享类型

## 配置

### 配置优先级

RCoder 支持多种配置方式，优先级从高到低为：

1. **命令行参数** - 最高优先级
2. **环境变量** - 中等优先级
3. **配置文件** - 较低优先级
4. **默认配置** - 最低优先级

### 配置文件

RCoder 支持通过 `config.yml` 文件进行配置。在首次启动时，系统会自动在当前目录下创建默认配置文件。

```yaml
# rcoder 配置文件

# 默认使用的 AI 代理类型
# 可选值: "Codex", "Claude" 
default_agent: Codex

# 项目工作的根目录
projects_dir: "./project_workspace"

# 服务端口
port: 3000
```

### 环境变量

以下环境变量会覆盖配置文件设置（但会被命令行参数覆盖）：

- `RCODER_PORT`: 服务器端口 (覆盖 config.yml 中的 port 设置)
- `DATABASE_URL`: 数据库连接字符串 (默认: sqlite:///./rcoder.db)
- `CLAUDE_CODE_PATH`: Claude Code CLI 路径 (默认: claude)
- `RUST_LOG`: 日志级别 (默认: info)

### 使用示例

```bash
# 使用命令行参数设置端口和项目目录
cargo run --bin rcoder -- --port 8080 --projects-dir /tmp/projects

# 使用环境变量覆盖端口
RCODER_PORT=8080 cargo run --bin rcoder

# 同时使用环境变量和命令行参数（命令行参数优先）
RCODER_PORT=8080 cargo run --bin rcoder -- --port 9000

# 使用自定义配置文件
cp config.yml.example config.yml
# 编辑 config.yml 并运行
cargo run --bin rcoder
```

### 配置文件

创建 `.env` 文件（可选）：

```env
RUST_LOG=debug
DATABASE_URL=sqlite:///./rcoder.db
CLAUDE_CODE_PATH=claude
```

## 🔧 开发指南

### 运行测试

```bash
# 运行所有测试
cargo test

# 运行特定模块测试
cargo test --package rcoder

# 运行集成测试
cargo test --test integration
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
RUST_LOG=debug cargo run --bin rcoder -- --port 8080

# 监视文件变化并自动重启
cargo install cargo-watch
cargo watch -x "run --bin rcoder"
```

## 🚀 部署指南

### Docker 部署

```dockerfile
# Dockerfile
FROM rust:1.75 as builder

WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/rcoder /usr/local/bin/rcoder
COPY --from=builder /app/config.yml.example /app/config.yml

WORKDIR /app
EXPOSE 3000

CMD ["rcoder"]
```

```bash
# 构建镜像
docker build -t rcoder:latest .

# 运行容器
docker run -p 3000:3000 \
  -e RCODER_PORT=3000 \
  -v $(pwd)/projects:/app/projects \
  rcoder:latest
```

### Docker Compose

```yaml
# docker-compose.yml
version: '3.8'

services:
  rcoder:
    build: .
    ports:
      - "3000:3000"
    environment:
      - RCODER_PORT=3000
      - RUST_LOG=info
    volumes:
      - ./projects:/app/projects
      - ./config.yml:/app/config.yml
    restart: unless-stopped

  # 可选：添加 nginx 反向代理
  nginx:
    image: nginx:alpine
    ports:
      - "80:80"
    volumes:
      - ./nginx.conf:/etc/nginx/nginx.conf
    depends_on:
      - rcoder
```

### 生产环境配置

```bash
# 使用 systemd 管理服务
sudo tee /etc/systemd/system/rcoder.service > /dev/null <<EOF
[Unit]
Description=RCoder AI Development Platform
After=network.target

[Service]
Type=simple
User=rcoder
WorkingDirectory=/opt/rcoder
ExecStart=/opt/rcoder/target/release/rcoder --port 3000
Restart=always
RestartSec=5
Environment=RUST_LOG=info
Environment=RCODER_PORT=3000

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl enable rcoder
sudo systemctl start rcoder
```

## 🔗 相关链接

- **项目仓库**: [GitHub](https://github.com/your-org/rcoder)
- **问题追踪**: [Issues](https://github.com/your-org/rcoder/issues)
- **贡献指南**: [CONTRIBUTING.md](CONTRIBUTING.md)
- **变更日志**: [CHANGELOG.md](CHANGELOG.md)
- **ACP 协议**: [Agent Client Protocol](https://github.com/zed-industries/zed/tree/main/crates/agent_client_protocol)
- **Claude Code**: [Anthropic Claude Code](https://docs.anthropic.com/claude/docs)
- **OpenAI Codex**: [OpenAI Codex Documentation](https://github.com/openai/codex)

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

## 🐛 问题排查

如果你遇到问题或有建议，请：

1. 查看 [FAQ](docs/FAQ.md)
2. 搜索现有的 [Issues](https://github.com/your-org/rcoder/issues)
3. 查看日志输出 (`RUST_LOG=debug cargo run`)
4. 创建新的 Issue 并提供详细信息

### 常见问题

- **端口被占用**: 使用 `--port` 参数指定其他端口：`cargo run --bin rcoder -- --port 8087`
- **AI 代理连接失败**: 检查 API 密钥和网络连接
- **配置文件错误**: 检查 YAML 格式和字段名称
- **Workspace 项目**: 在 workspace 项目中必须使用 `--bin rcoder` 指定二进制名称

## 📈 更新日志

### v0.1.0 (当前版本)

#### 新增功能
- ✅ 初始版本发布
- ✅ 基于 ACP 协议的 AI 代理统一管理
- ✅ HTTP API 接口支持
- ✅ Claude Code 和 OpenAI Codex 代理集成
- ✅ YAML 配置文件支持
- ✅ 命令行参数支持
- ✅ 多层配置系统（命令行 > 环境变量 > 配置文件 > 默认）
- ✅ OpenTelemetry 集成和分布式追踪
- ✅ Swagger UI API 文档
- ✅ 项目文件解析器 (Nuwax Parser)

#### 技术特性
- ✅ 基于 Rust 2024 Edition
- ✅ 异步架构 (Tokio)
- ✅ 模块化设计 (Workspace Crates)
- ✅ 实时通信 (SSE)
- ✅ 结构化日志 (Tracing)

---

💫 **由 RCoder 团队精心打造，致力于推进 AI 驱动的现代化开发体验。**