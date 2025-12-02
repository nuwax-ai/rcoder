# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

RCoder 是一个基于 ACP (Agent Client Protocol) 的 AI 驱动开发平台，使用 Rust 构建。该项目采用微服务架构，集成了 Docker 容器化部署、高性能反向代理和多种 AI 代理支持。

## 核心架构

### 工作空间结构
- **Workspace**: 使用 Cargo workspace 管理多个 crate
- **主要 crate**: `rcoder` (主应用), `agent_runner` (代理运行时), `shared_types` (共享类型), `docker_manager` (容器管理), `pingora-proxy` (反向代理)

### 容器化架构设计
项目采用动态容器化架构，每个项目对应一个独立的 Docker 容器：
- **RCoder 主服务**: HTTP API 服务 + 容器管理
- **Agent Runner 容器**: 每个项目独立的 AI 代理运行环境
- **Pingora 代理**: 高性能反向代理服务，支持端口路由

### 核心组件
- **DockerManager**: 全局容器管理器，负责容器生命周期
- **ContainerManager**: 项目级别的容器创建和管理
- **ProxyAgentManager**: ACP 代理管理器，处理代理生命周期
- **AppState**: 应用状态管理，使用 DashMap 进行并发访问

## 开发命令

### 基础构建和运行
```bash
# 构建所有 crates
cargo build --release

# 运行主服务 (默认端口 8087)
cargo run --bin rcoder

# 运行特定端口
cargo run --bin rcoder -- --port 8080 --enable-proxy

# 使用 Makefile (推荐)
make build          # 本地编译
make install        # 安装到 ~/.cargo/bin
make dev-build      # Docker 镜像构建
make dev-up         # 启动开发容器
```

### 开发环境命令
```bash
# 启动开发模式容器
make dev-build      # 首次：构建 Docker 镜像
make dev-up         # 启动容器
make dev-restart    # 代码修改后重启容器

# 查看容器日志
make dev-logs

# 停止开发容器
make dev-down
```

### 测试和质量检查
```bash
# 运行所有测试
cargo test

# 运行特定 crate 测试
cargo test -p rcoder
cargo test -p docker_manager

# 代码质量
cargo fmt           # 格式化代码
cargo clippy         # 代码检查
cargo tree           # 查看依赖树
```

### Docker 开发命令
```bash
# 构建 Docker 镜像
make docker-build

# 完整开发流程 (推荐)
make dev-build && make dev-up

# 更新镜像标签
make update-image-tag
```

## 重要技术细节

### ACP 协议集成
- 使用 `agent-client-protocol = "0.6"` 和 `agent_client_protocol = "0.4"` 实现多版本兼容
- AgentSideConnection 和 ClientSideConnection **未实现 Send trait**
- **必须**在 LocalSet 和 spawn_local 中使用这些连接
- 参考示例目录: `/Volumes/soddy/git_workspace/rcoder/tmp/agent-client-protocol/rust/examples`

### 并发模型和状态管理
- 使用 **DashMap** 替代 `Arc<RwLock<HashMap>>` 以获得更好的性能
- 使用写时复制 (CoW) 模式进行状态更新
- 主应用使用 `#[tokio::main(flavor = "current_thread")]`
- ACP 操作必须在 `LocalSet` 中执行以支持 `spawn_local`

### Docker 容器动态创建
- **项目级隔离**: 每个项目对应一个独立的 Docker 容器
- **自动架构检测**: 根据 OS 和 ARCH 自动选择合适的镜像
- **内部网络通信**: 容器间通过 Docker 内部网络直接通信，无需端口映射
- **路径自动解析**: 自动检测容器内路径到宿主机路径的映射

### 配置系统
多层级配置优先级 (从高到低):
1. **命令行参数** - `--port`, `--projects-dir`, `--enable-proxy`
2. **环境变量** - `RCODER_PORT`, `DOCKER_SOCKET_PATH`, `RCODER_DOCKER_IMAGE_*`
3. **配置文件** - `config.yml` (自动生成)
4. **默认配置** - 代码中的默认值

## 环境配置

### 核心环境变量
```bash
# 服务配置
RCODER_PORT=8087                           # 服务端口
RUST_LOG=debug                            # 日志级别

# Docker 配置
DOCKER_SOCKET_PATH=/var/run/docker.sock     # Docker socket 路径
RCODER_DOCKER_IMAGE=custom/image          # 自定义镜像
RCODER_DOCKER_IMAGE_ARM64=arm64/image     # ARM64 专用镜像
RCODER_DOCKER_IMAGE_AMD64=amd64/image     # AMD64 专用镜像

# 代理配置
ANTHROPIC_API_KEY=sk-xxx                 # Claude API 密钥
COMPOSE_PROJECT_NAME=rcoder                 # Docker Compose 项目名
```

### 开发环境要求
- Rust 1.75+ (2024 Edition)
- Docker 和 Docker Compose
- Claude Code CLI (可选)

## API 接口

### 核心端点
- `POST /chat`: 发送聊天消息到 AI 代理
- `GET /agent/progress/{session_id}`: SSE 进度流，接收实时通知
- `POST /agent/session/cancel`: 取消正在执行的任务
- `POST /agent/stop`: 停止 Agent
- `GET /agent/status/{project_id}`: 查询 Agent 状态
- `GET /health`: 健康检查

### Pingora 反向代理
- `GET /proxy/{port}/{path}`: 端口路由到指定后端服务
- `GET /proxy/status`: 查看代理服务状态
- `GET /proxy/stats`: 查看代理统计信息

### 响应格式
所有 API 响应都使用统一的 HttpResult 格式：
```rust
struct HttpResult<T> {
    success: bool,
    data: Option<T>,
    code: String,        // 业务错误码
    message: String,     // 错误描述
    tid: Option<String>, // 追踪ID
}
```

## 特殊注意事项

### 禁止事项
1. **禁止使用模拟响应逻辑** - 所有 AI 调用必须真实执行
2. **禁止编写 unsafe 代码** - 项目要求内存安全
3. **AgentSideConnection 必须在 LocalSet 中使用** - 由于未实现 Send trait
4. ** Always Response in 中文** - 所有响应必须使用中文

### Docker 容器管理
- **容器名称格式**: `rcoder-agent-{project_id}`
- **镜像选择策略**: 通用镜像 > 架构特定镜像 > 默认回退镜像
- **网络模式**: 优先使用内部网络，支持 host 网络模式
- **安全配置**: 自动移除 NET_RAW 和 NET_ADMIN 权限

### 性能优化
- 使用 DashMap 进行并发访问，避免 RwLock 竞争
- 实现写时复制 (CoW) 模式，减少不必要的内存分配
- 使用 MPMC 架构处理多个 AI 请求
- 通过内部网络进行容器间通信，避免宿主机端口映射

### 错误处理
- 使用 anyhow 进行错误传播
- 使用 HttpResult 统一 API 响应格式
- 实现完整的错误追踪和日志记录

## 调试和开发

### 日志配置
```bash
# 启用详细日志
RUST_LOG=debug cargo run --bin rcoder

# 特定模块日志
RUST_LOG=rcoder=debug,tower_http=debug cargo run

# 在容器中启用调试
RUST_LOG=debug make dev-up
```

### 容器调试
```bash
# 查看容器状态
docker ps | grep rcoder

# 查看容器日志
make dev-logs

# 进入容器调试
docker exec -it <container_id> /bin/bash
```

### 网络调试
```bash
# 检查容器网络
docker network ls
docker network inspect rcoder_agent-network

# 测试容器间连通性
docker exec <container1> ping <container2_ip>
```

## 开发工作流程

1. **首次开发环境设置**:
   ```bash
   make dev-build      # 构建 Docker 镜像
   make dev-up         # 启动开发容器
   ```

2. **日常开发**:
   ```bash
   # 修改代码后
   make dev-restart    # 重新编译并重启容器
   ```

3. **测试新功能**:
   ```bash
   # 直接运行
   cargo run --bin rcoder -- --port 8080
   
   # 或使用容器
   make dev-up
   curl -X POST http://localhost:8087/chat -d '{"prompt":"hello"}'
   ```

4. **调试问题**:
   ```bash
   # 查看详细日志
   make dev-logs
   RUST_LOG=debug make dev-restart
   ```

## 关键代码模式

### ACP 协议集成模式
```rust
// 正确的 LocalSet 使用模式
let local_set = LocalSet::new();
local_set.run_until(async move {
    let (client_conn, handle_io) = ClientSideConnection::new(
        client, outgoing, incoming, |fut| {
            tokio::task::spawn_local(fut);
        }
    );
    tokio::task::spawn_local(handle_io);
    // ... 处理逻辑
}).await;
```

### DashMap 高效使用模式
```rust
// 使用 entry API 避免多次锁获取
let entry = state.project_and_agent_map.entry(project_id.clone());
match entry {
    dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
        // 只在需要更新时进行写时复制
        if needs_update {
            let mut mutable_info = (**occupied.get()).clone();
            mutable_info.update_field(value);
            occupied.insert(Arc::new(mutable_info));
        }
    }
    dashmap::mapref::entry::Entry::Vacant(vacant) => {
        // 创建新条目
        let new_info = ProjectAndContainerInfo::new(project_id);
        vacant.insert(Arc::new(new_info));
    }
}
```

### Docker 容器创建模式
```rust
// 容器配置模式
let container_config = DockerContainerConfig {
    project_id: project_id.clone(),
    image: get_docker_image_from_config(image, arm64_image, amd64_image, default_image),
    host_path: resolve_container_path_to_host(&project_path).await?,
    container_path: project_path.clone(),
    port_bindings: HashMap::new(), // 内部网络，无需端口映射
    network_name: Some(network_name),
    // ... 其他配置
};
```