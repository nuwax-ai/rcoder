# RCoder - AI-Powered Development Platform

RCoder 是一个基于 ACP (Agent Client Protocol) 的 AI 驱动开发平台，让用户可以通过简单的 HTTP 请求来创建、管理和开发软件项目。

## 特性

- 🤖 **AI 驱动开发**: 通过 Claude Code 进行智能代码生成和项目管理
- 🌐 **HTTP API**: 简单易用的 RESTful API 接口
- 🏗️ **项目模板**: 支持多种编程语言的项目模板
- 📁 **文件管理**: 自动文件创建、修改和删除
- 🔄 **实时同步**: 与 AI Agent 的实时通信
- 🗄️ **持久化存储**: 基于 SQLite 的项目和会话管理

## 架构概览

```
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│   HTTP API      │    │   Project Mgr   │    │  Claude Code    │
│   (Axum)        │◄──►│   (SQLite)      │◄──►│   (ACP)         │
└─────────────────┘    └─────────────────┘    └─────────────────┘
         ▲                        ▲                        ▲
         │                        │                        │
         ▼                        ▼                        ▼
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│   Web Client    │    │   ACP Client    │    │   Agent Process  │
│   (Browser)     │    │   Protocol      │    │   (Claude CLI)  │
└─────────────────┘    └─────────────────┘    └─────────────────┘
```

## 快速开始

### 环境要求

- Rust 1.70+
- Claude Code CLI
- SQLite 3

### 安装

1. 克隆项目
```bash
git clone <repository-url>
cd rcoder
```

2. 构建项目
```bash
cargo build --release
```

3. 运行服务
```bash
cargo run --release
```

服务将在 `http://localhost:3000` 启动。

### 使用 Claude Code

确保已安装 Claude Code CLI：

```bash
# 安装 Claude Code
npm install -g @anthropic-ai/claude-code

# 或使用官方安装方法
# 参考: https://docs.anthropic.com/claude/docs/getting-started
```

## API 文档

### 健康检查
```bash
GET /api/health
```

### 项目管理

#### 列出项目
```bash
GET /api/projects
```

#### 获取项目详情
```bash
GET /api/projects/{project-id}
```

#### 更新项目
```bash
PUT /api/projects/{project-id}
Content-Type: application/json

{
  "name": "updated-name",
  "description": "Updated description"
}
```

#### 删除项目
```bash
DELETE /api/projects/{project-id}
```

### 智能开发接口

#### 发送提示（自动创建项目）
```bash
POST /api/prompts
Content-Type: application/json

{
  "prompt": "Create a Rust web API project with user management",
  "auto_create": true
}
```

#### 发送提示（指定现有项目）
```bash
POST /api/prompts
Content-Type: application/json

{
  "project_id": "existing-project-uuid",
  "prompt": "Add a new REST API endpoint for users",
  "context": {
    "files": ["src/main.rs"],
    "current_file": "src/main.rs"
  }
}
```

#### 获取项目文件
```bash
GET /api/projects/{project-id}/files
```

#### 获取项目统计
```bash
GET /api/projects/{project-id}/stats
```

### 模板

#### 列出模板
```bash
GET /api/templates
```

#### 获取模板详情
```bash
GET /api/templates/{template-name}
```

## 项目结构

```
rcoder/
├── src/                    # 主应用程序
│   └── main.rs
├── crates/                 # Workspace crates
│   ├── shared_types/       # 共享类型定义
│   ├── acp_client/         # ACP 客户端实现
│   ├── claude_integration/ # Claude Code 集成
│   ├── project_manager/    # 项目管理
│   └── http_server/        # HTTP 服务器
├── migrations/             # 数据库迁移
├── projects/               # 生成的项目
└── Cargo.toml
```

## 配置

### 环境变量

- `PORT`: 服务器端口 (默认: 3000)
- `DATABASE_URL`: 数据库连接字符串 (默认: sqlite:///./rcoder.db)
- `CLAUDE_CODE_PATH`: Claude Code CLI 路径 (默认: claude)
- `RUST_LOG`: 日志级别 (默认: info)

### 配置文件

创建 `.env` 文件：

```env
PORT=3000
DATABASE_URL=sqlite:///./rcoder.db
CLAUDE_CODE_PATH=claude
RUST_LOG=debug
```

## 开发

### 运行测试

```bash
cargo test
```

### 代码格式化

```bash
cargo fmt
```

### 代码检查

```bash
cargo clippy
```

## 部署

### 使用 Docker

```dockerfile
FROM rust:1.70 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bullseye-slim
RUN apt-get update && apt-get install -y ca-certificates
COPY --from=builder /app/target/release/rcoder /usr/local/bin/
CMD ["rcoder"]
```

### 使用 Docker Compose

```yaml
version: '3.8'
services:
  rcoder:
    build: .
    ports:
      - "3000:3000"
    environment:
      - DATABASE_URL=sqlite:///./data/rcoder.db
    volumes:
      - ./data:/app/data
```

## 许可证

本项目采用 MIT 许可证。详见 [LICENSE](LICENSE) 文件。

## 贡献

欢迎贡献！请阅读 [CONTRIBUTING.md](CONTRIBUTING.md) 了解如何参与开发。

## 支持

如果你遇到问题或有建议，请：

1. 查看 [文档](docs/)
2. 搜索现有的 [Issues](issues)
3. 创建新的 Issue

## 更新日志

### v0.1.0

- 初始版本
- 基础 ACP 协议支持
- HTTP API 接口
- 项目管理功能
- Claude Code 集成
- 多种项目模板