# RCoder Docker Agent 使用指南

本指南介绍如何在 RCoder 项目中启用和使用 Docker Agent 功能。

## 概述

Docker Agent 是 RCoder 项目的一个新功能，它允许在 Docker 容器中运行 AI Agent，提供更好的环境隔离和资源管理。

### 主要优势

- 🏗️ **环境隔离**: 每个项目在独立的 Docker 容器中运行
- 🔄 **动态管理**: 根据需要动态创建和销毁容器
- 📁 **目录挂载**: 自动挂载项目工作目录到容器
- 🛠️ **工具链完整**: 容器包含完整的 AI 开发工具链
- 📊 **资源监控**: 支持容器状态监控和日志获取

## 快速开始

### 1. 环境准备

确保系统已安装并运行 Docker：

```bash
# 检查 Docker 是否安装
docker --version

# 检查 Docker 是否运行
docker info
```

### 2. 启用 Docker Agent

通过环境变量启用 Docker Agent：

```bash
export USE_DOCKER_AGENT=true
```

### 3. 启动 RCoder 服务

```bash
cargo run --release
```

## 详细配置

### 环境变量配置

| 环境变量 | 默认值 | 说明 |
|---------|-------|------|
| `USE_DOCKER_AGENT` | `false` | 是否启用 Docker Agent |
| `DOCKER_HOST` | - | Docker 守护进程地址 |
| `DEFAULT_DOCKER_IMAGE` | `registry.yichamao.com/rcoder:latest` | 默认 Docker 镜像 |
| `DOCKER_NETWORK_MODE` | `host` | Docker 网络模式 |
| `DOCKER_WORK_DIR` | `/app/workspace` | 容器内工作目录 |
| `PROJECTS_DIR` | `./project_workspace` | 项目工作目录 |

### 项目目录结构

```
./project_workspace/
├── project_123/          # 项目 123 的工作目录
│   ├── src/
│   ├── Cargo.toml
│   ├── README.md
│   └── ...
├── project_456/          # 项目 456 的工作目录
│   ├── main.py
│   ├── requirements.txt
│   └── ...
└── ...
```

### Docker 镜像

使用预构建的 RCoder Docker 镜像：

- **镜像地址**: `registry.yichamao.com/rcoder:latest`
- **基础镜像**: Node.js 22
- **包含工具**:
  - Rust 工具链
  - Node.js 和 npm/pnpm
  - Claude Code CLI
  - OpenAI Codex CLI
  - 各种开发工具

## 使用方式

### 1. 通过 API 使用

#### 创建聊天请求

```bash
curl -X POST http://localhost:3000/chat \
  -H "Content-Type: application/json" \
  -d '{
    "project_id": "my_project",
    "prompt": "创建一个简单的 Rust 应用",
    "model_provider": {
      "name": "anthropic",
      "base_url": "https://api.anthropic.com",
      "api_key": "your_api_key",
      "default_model": "claude-3-5-sonnet-20241022"
    }
  }'
```

#### 获取执行进度

```bash
curl -N http://localhost:3000/agent/progress/my_project
```

### 2. 通过环境变量控制

#### 强制使用 Docker Agent

```bash
export USE_DOCKER_AGENT=true
```

#### 使用特定 Docker 镜像

```bash
export DEFAULT_DOCKER_IMAGE="custom/rcoder:v1.0.0"
```

#### 配置项目目录

```bash
export PROJECTS_DIR="/path/to/your/projects"
```

## Agent 类型选择

RCoder 支持三种 Agent 类型，选择优先级如下：

1. **Docker Agent**: 当 `USE_DOCKER_AGENT=true` 时强制使用
2. **Claude Code**: Anthropic 协议时使用
3. **Codex**: OpenAI 协议或默认时使用

### 选择逻辑

```rust
// 伪代码展示 Agent 类型选择逻辑
if USE_DOCKER_AGENT == "true" {
    AgentType::Docker
} else if model_provider.protocol == "anthropic" {
    AgentType::Claude
} else {
    AgentType::Codex
}
```

## 容器管理

### 自动管理

Docker Agent 会自动管理容器的生命周期：

1. **创建**: 收到请求时自动创建容器
2. **复用**: 同一项目的后续请求复用现有容器
3. **更新**: 模型配置变化时重启容器
4. **清理**: 服务停止时自动清理容器

### 手动管理

如果需要手动管理容器，可以使用以下方法：

#### 查看活跃容器

```bash
docker ps | grep rcoder-docker-agent
```

#### 查看容器日志

```bash
docker logs <container_id>
```

#### 停止特定容器

```bash
docker stop <container_id>
```

## 监控和调试

### 日志级别

设置不同的日志级别获取详细信息：

```bash
# 信息级别
export RUST_LOG=info

# 调试级别
export RUST_LOG=debug

# 跟踪级别
export RUST_LOG=trace
```

### 常见问题排查

#### 1. Docker 连接失败

```bash
# 检查 Docker 是否运行
docker info

# 检查 Docker socket 权限
ls -la /var/run/docker.sock

# 如果权限问题，添加用户到 docker 组
sudo usermod -aG docker $USER
```

#### 2. 镜像拉取失败

```bash
# 手动拉取镜像
docker pull registry.yichamao.com/rcoder:latest

# 检查网络连接
ping registry.yichamao.com
```

#### 3. 容器启动失败

```bash
# 查看容器日志
docker logs <container_id>

# 检查项目目录权限
ls -la ./project_workspace/
```

#### 4. 端口冲突

```bash
# 检查端口占用
netstat -tulpn | grep :3000

# 修改服务端口
export PORT=3001
```

## 性能优化

### 1. 资源限制

可以为 Docker Agent 设置资源限制：

```rust
// 在 DockerContainerConfig 中设置
let config = DockerContainerConfig {
    // ...
    resource_limits: Some(ResourceLimits {
        memory_limit: Some(2 * 1024 * 1024 * 1024), // 2GB
        cpu_limit: Some(1.0),                        // 1 CPU
        swap_limit: Some(4 * 1024 * 1024 * 1024),   // 4GB
    }),
    ..
};
```

### 2. 网络优化

使用 host 网络模式以获得更好的性能：

```bash
export DOCKER_NETWORK_MODE=host
```

### 3. 存储优化

使用 SSD 存储项目目录以提高 I/O 性能：

```bash
# 将项目目录放在快速存储上
export PROJECTS_DIR="/ssd/project_workspace"
```

## 安全考虑

### 1. 容器隔离

虽然 Docker 提供了一定的隔离，但仍需注意：

- 不要在容器中运行敏感命令
- 定期更新 Docker 镜像
- 限制容器的系统调用

### 2. API 密钥管理

- 使用环境变量传递 API 密钥
- 不要在日志中输出敏感信息
- 定期轮换 API 密钥

### 3. 网络安全

- 如果不需要外部访问，使用 bridge 网络模式
- 限制容器的网络访问权限
- 使用防火墙规则限制访问

## 故障排除

### 收集诊断信息

当遇到问题时，收集以下信息：

```bash
# 系统信息
uname -a
docker --version
docker info

# RCoder 服务日志
journalctl -u rcoder -f

# Docker 容器状态
docker ps -a
docker images

# 网络连接
netstat -tulpn
```

### 常见错误及解决方案

#### Error: "无法连接到 Docker 守护进程"

**原因**: Docker 服务未运行或权限问题

**解决方案**:
```bash
# 启动 Docker 服务
sudo systemctl start docker
sudo systemctl enable docker

# 或者在 macOS/Windows 上启动 Docker Desktop
```

#### Error: "镜像拉取失败"

**原因**: 网络问题或认证问题

**解决方案**:
```bash
# 检查网络连接
curl -I https://registry.yichamao.com

# 手动登录镜像仓库
docker login registry.yichamao.com
```

#### Error: "项目目录不存在"

**原因**: 项目工作目录未创建

**解决方案**:
```bash
# 创建项目目录
mkdir -p ./project_workspace
chmod 755 ./project_workspace
```

## 最佳实践

1. **资源管理**: 及时清理不用的容器和镜像
2. **监控**: 定期检查容器状态和资源使用情况
3. **备份**: 定期备份项目目录
4. **更新**: 保持 Docker 镜像和 RCoder 版本更新
5. **安全**: 遵循最小权限原则配置容器权限

## 示例项目

查看 `crates/docker_manager/examples/` 目录中的示例代码：

- `basic_usage.rs`: 基本 Docker 管理功能演示
- `rcoder_integration.rs`: 与 RCoder 系统集成示例

运行示例：

```bash
# 基本使用示例
cargo run --example basic_usage --package docker_manager

# RCoder 集成示例
cargo run --example rcoder_integration --package rcoder
```

## 支持

如果遇到问题或需要帮助：

1. 查看日志文件获取详细错误信息
2. 检查 GitHub Issues 中的已知问题
3. 提交新的 Issue 并提供详细的错误信息和环境信息
4. 联系开发团队获取技术支持

---

更多信息请参考：
- [Docker 官方文档](https://docs.docker.com/)
- [RCoder 项目文档](../README.md)
- [API 文档](../api-docs.md)