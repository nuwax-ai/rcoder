# Agent Server

RCoder Docker 容器内的 Agent 服务管理器，提供完整的 AI Agent 生命周期管理功能。

## 概述

Agent Server 是专门为在 Docker 容器内运行而设计的 Agent 服务管理器。它提供了与 RCoder 主服务相同的 API 接口，支持：

- 🤖 **Agent 管理**: 支持启动、停止、重启 Agent 实例
- 💬 **聊天处理**: 完整的聊天请求处理流程
- 📡 **进度通知**: 基于 SSE 的实时进度推送
- 🛠️ **生命周期管理**: 优雅的启动和关闭机制
- 🏥 **健康检查**: 完整的健康监控和状态检查

## 核心功能

### 1. Agent 服务管理

```rust
use agent_server::{AgentServer, AgentServerConfig};

// 创建配置
let config = AgentServerConfig {
    port: 8086,
    agent_type: AgentType::Claude,
    project_id: "my_project".to_string(),
    work_dir: "/app/workspace".into(),
    ..Default::default()
};

// 创建并启动服务器
let server = AgentServer::new(config).await?;
server.start().await?;
```

### 2. HTTP API 接口

Agent Server 提供与 RCoder 主服务兼容的 API 接口：

#### 健康检查
```bash
GET /health
```

#### 聊天接口
```bash
POST /chat
Content-Type: application/json

{
  "prompt": "帮我写一个 Rust 程序",
  "project_id": "my_project",
  "model_provider": {
    "name": "anthropic",
    "api_key": "your_key",
    "default_model": "claude-3-5-sonnet-20241022"
  }
}
```

#### Agent 进度通知 (SSE)
```bash
GET /agent/progress/{session_id}
Accept: text/event-stream
```

#### Agent 管理接口
```bash
# 取消请求
POST /agent/session/cancel

# 停止 Agent
POST /agent/stop

# 获取状态
GET /agent/status/{project_id}
```

### 3. Docker 集成

Agent Server 设计为在 Docker 容器内运行，通过环境变量进行配置：

```bash
# 基本配置
export AGENT_SERVER_PORT=8086
export AGENT_TYPE=claude
export PROJECT_ID=my_project
export WORK_DIR=/app/workspace

# 模型配置
export ANTHROPIC_AUTH_TOKEN=your_token
export ANTHROPIC_MODEL=claude-3-5-sonnet-20241022

# 启动服务
./agent-server start
```

## 架构设计

### 组件结构

```
Agent Server
├── AgentManager          # Agent 实例管理
├── ApiRouter             # HTTP 路由和处理器
├── SessionManager        # 会话管理
├── ProgressNotifier      # 进度通知 (SSE)
├── HealthChecker         # 健康检查
└── ShutdownManager       # 优雅关闭管理
```

### 生命周期

1. **初始化阶段**:
   - 解析配置参数
   - 初始化 Agent 实例
   - 启动 HTTP 服务器

2. **运行阶段**:
   - 处理 API 请求
   - 管理 Agent 会话
   - 推送进度通知

3. **关闭阶段**:
   - 接收关闭信号
   - 优雅停止所有会话
   - 关闭 HTTP 服务器

## 配置选项

### 命令行参数

```bash
# 基本启动
./agent-server start \
  --port 8086 \
  --agent-type claude \
  --project-id my_project \
  --work-dir /app/workspace

# 停止服务
./agent-server stop

# 重启服务
./agent-server restart

# 检查状态
./agent-server status
```

### 环境变量配置

| 变量名 | 默认值 | 说明 |
|--------|--------|------|
| `AGENT_SERVER_PORT` | 8086 | 服务端口 |
| `AGENT_TYPE` | claude | Agent 类型 (claude/codex) |
| `PROJECT_ID` | - | 项目 ID (必需) |
| `WORK_DIR` | /app/workspace | 工作目录 |
| `SESSION_ID` | - | 会话 ID (可选) |
| `RUST_LOG` | info | 日志级别 |
| `MAX_SESSIONS` | 10 | 最大会话数 |
| `SESSION_TIMEOUT_SECS` | 3600 | 会话超时时间 |

### 模型提供商配置

#### Claude Code Agent
```bash
export ANTHROPIC_AUTH_TOKEN=your_token
export ANTHROPIC_BASE_URL=https://api.anthropic.com
export ANTHROPIC_MODEL=claude-3-5-sonnet-20241022
export CLAUDE_CODE_ARGS="--dangerously-skip-permissions"
```

#### Codex Agent
```bash
export OPENAI_API_KEY=your_key
export OPENAI_API_BASE=https://api.openai.com/v1
export OPENAI_MODEL=gpt-4
export CODEX_API_KEY=your_key
```

## Docker 集成

### Dockerfile 配置

```dockerfile
# 使用 RCoder 基础镜像
FROM registry.yichamao.com/rcoder:latest

# 复制 Agent Server 二进制文件
COPY --from=agent-builder /app/bin/agent-server /app/bin/agent-server

# 复制启动脚本
COPY docker-entrypoint.sh /app/docker-entrypoint.sh
RUN chmod +x /app/docker-entrypoint.sh

# 设置工作目录
WORKDIR /app

# 暴露端口
EXPOSE 8086

# 设置入口点
ENTRYPOINT ["/app/docker-entrypoint.sh"]
```

### Docker Compose 示例

```yaml
version: '3.8'

services:
  agent-server:
    image: registry.yichamao.com/rcoder:latest
    container_name: rcoder-agent-server
    ports:
      - "8086:8086"
    environment:
      - AGENT_SERVER_PORT=8086
      - AGENT_TYPE=claude
      - PROJECT_ID=docker_project
      - WORK_DIR=/app/workspace
      - RUST_LOG=info
      - ANTHROPIC_AUTH_TOKEN=${ANTHROPIC_TOKEN}
      - ANTHROPIC_MODEL=claude-3-5-sonnet-20241022
    volumes:
      - ./project_workspace:/app/workspace
    restart: unless-stopped
    networks:
      - rcoder-network

networks:
  rcoder-network:
    driver: bridge
```

## 使用示例

### 1. 基本使用

```rust
use agent_server::{AgentServer, AgentServerConfig, AgentType};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = AgentServerConfig {
        port: 8086,
        agent_type: AgentType::Claude,
        project_id: "example_project".to_string(),
        work_dir: "/app/workspace".into(),
        ..Default::default()
    };

    let server = AgentServer::new(config).await?;
    server.start().await?;

    Ok(())
}
```

### 2. 与 Docker Manager 集成

```rust
use docker_manager::docker_agent::{DockerAgentManager, AgentType};

#[tokio::main]
async fn main() -> Result<()> {
    let agent_manager = DockerAgentManager::new().await?;

    // 创建 Docker Agent
    let agent_info = agent_manager.create_docker_agent(
        "my_project",
        AgentType::Claude,
        "./project_workspace",
        Some(model_provider_config),
    ).await?;

    // 发送聊天请求
    let response = agent_manager.send_chat_request("my_project", chat_request).await?;

    // 停止 Agent
    agent_manager.stop_docker_agent("my_project").await?;

    Ok(())
}
```

### 3. SSE 进度通知

```javascript
// 前端连接 SSE
const eventSource = new EventSource('/agent/progress/session_123');

eventSource.onmessage = function(event) {
    const data = JSON.parse(event.data);
    console.log('进度更新:', data);
};

eventSource.onerror = function(error) {
    console.error('SSE 错误:', error);
};
```

## 监控和调试

### 健康检查

```bash
# 基本健康检查
curl http://localhost:8086/health

# 详细状态检查
curl http://localhost:8086/agent/status/my_project
```

### 日志配置

```bash
# 设置调试日志级别
export RUST_LOG=debug

# 启用文件日志
export LOG_FILE=/app/logs/agent-server.log

# 启动服务
./agent-server start
```

### 性能监控

Agent Server 提供内置的性能监控：

- **CPU 使用率**: 实时监控 CPU 占用
- **内存使用量**: 跟踪内存分配和使用
- **活跃会话数**: 监控并发会话数量
- **请求处理量**: 统计处理的请求数量

## 故障排除

### 常见问题

1. **端口冲突**
   ```bash
   # 检查端口占用
   netstat -tulpn | grep 8086

   # 使用不同端口
   export AGENT_SERVER_PORT=8087
   ```

2. **权限问题**
   ```bash
   # 检查工作目录权限
   ls -la /app/workspace

   # 修复权限
   chmod 755 /app/workspace
   ```

3. **Agent 启动失败**
   ```bash
   # 检查日志
   tail -f /app/logs/agent-server.log

   # 验证配置
   ./agent-server status
   ```

4. **网络连接问题**
   ```bash
   # 检查网络连通性
   curl -I http://localhost:8086/health

   # 检查防火墙规则
   iptables -L | grep 8086
   ```

### 调试模式

```bash
# 启用详细日志
export RUST_LOG=debug

# 启用调试模式
export DEBUG_MODE=true

# 启动调试会话
./agent-server start --debug
```

## 性能优化

### 1. 配置优化

```rust
let config = AgentServerConfig {
    max_sessions: 50,           // 增加最大会话数
    session_timeout_secs: 7200,  // 延长会话超时时间
    request_timeout_secs: 180,   // 增加请求超时时间
    max_request_size_bytes: 20 * 1024 * 1024, // 增加请求大小限制
    ..Default::default()
};
```

### 2. 资源限制

```bash
# 限制内存使用
export AGENT_MEMORY_LIMIT=2g

# 限制 CPU 使用
export AGENT_CPU_LIMIT=1.0

# 限制文件描述符
export AGENT_NOFILE_LIMIT=10000
```

### 3. 缓存策略

Agent Server 实现了多级缓存：

- **会话缓存**: 缓存活跃会话信息
- **响应缓存**: 缓存常见请求的响应
- **连接池**: 复用 HTTP 连接

## 安全考虑

### 1. 认证和授权

```bash
# 设置 API 密钥
export AGENT_API_KEY=your_secret_key

# 启用 TLS
export AGENT_ENABLE_TLS=true
export AGENT_TLS_CERT=/app/certs/server.crt
export AGENT_TLS_KEY=/app/certs/server.key
```

### 2. 网络安全

- 使用 HTTPS 进行通信
- 限制访问来源 IP
- 启用请求速率限制
- 定期更新依赖库

### 3. 数据安全

- 不在日志中记录敏感信息
- 定期清理临时文件
- 加密存储的配置信息

## 最佳实践

1. **资源配置**: 根据负载合理配置资源限制
2. **监控告警**: 设置健康检查和告警机制
3. **日志管理**: 实施日志轮转和归档策略
4. **版本管理**: 使用容器镜像版本标签管理
5. **备份恢复**: 定期备份重要配置和数据

## 贡献指南

欢迎贡献代码和文档！请遵循以下步骤：

1. Fork 项目仓库
2. 创建功能分支
3. 提交代码变更
4. 编写测试用例
5. 提交 Pull Request

更多信息请参考：
- [RCoder 主项目文档](../../README.md)
- [Docker Manager 文档](../docker_manager/README.md)
- [API 文档](../docs/api.md)