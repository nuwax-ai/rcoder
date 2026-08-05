# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

RCoder 是一个基于 ACP (Agent Client Protocol) 的 AI 驱动开发平台，使用 Rust 构建。该项目采用微服务架构，集成了 Docker 容器化部署、高性能反向代理和多种 AI 代理支持。

## 核心架构

### 工作空间结构
- **Workspace**: 使用 Cargo workspace 管理多个 crate
- **主要 crate**: `rcoder` (主应用), `agent_runner` (代理运行时), `docker_manager` (容器管理), `rcoder-proxy` (反向代理), `shared_types` (共享类型), `shared_types_grpc` (gRPC proto 定义), `agent_abstraction` (ACP 代理连接), `app_manager` (UserApp 管理), `container-runtime-api` (容器运行时抽象), `file-server` (独立文件服务), `rcoder-telemetry` (可观测性)

### 容器化架构设计
项目采用动态容器化架构，每个项目对应一个独立的 Docker 容器：
- **RCoder 主服务**: HTTP API 服务 + 容器管理 + gRPC 客户端
- **Agent Runner 容器**: 每个项目独立的 AI 代理运行环境 + gRPC 服务器
- **Pingora 代理**: 高性能反向代理服务，支持端口路由

### 通信架构（gRPC）
RCoder 和 Agent Runner 之间使用 **gRPC** 进行内部通信，提供类型安全和高性能：

```
外部客户端 (HTTP/SSE)
    ↓
RCoder (HTTP API Server)
    ↓ gRPC (Chat, CancelSession, SubscribeProgress)
Agent Runner (gRPC Server in Docker)
    ↓ Server Streaming (实时进度事件)
RCoder (转换为 SSE)
    ↓
外部客户端 (SSE)
```

**核心 RPC 方法**：
- `Chat`: 发送聊天请求到 agent_runner
- `SubscribeProgress`: Server Streaming，实时推送进度事件
- `CancelSession`: 取消正在执行的会话
- `GetStatus`: 查询 Agent 状态

**Proto 定义位置**: `crates/shared_types_grpc/proto/agent.proto`

### 核心组件
- **DockerManager**: 全局容器管理器，负责容器生命周期
- **ContainerManager**: 项目级别的容器创建和管理
- **ProxyAgentManager**: ACP 代理管理器，处理代理生命周期
- **AppState**: 应用状态管理，使用 DashMap 进行并发访问
- **GrpcChannelPool**: gRPC 连接池，基于 DashMap 实现高效连接复用
- **AgentServiceImpl**: agent_runner 的 gRPC 服务实现

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
# 首次部署（端口 8090）
make dev-build      # 构建 Docker 镜像 dev-master-rcoder:latest
make dev-up         # 启动容器

# 日常开发：改 Rust 源码后秒级热编译（推荐！详见下文「本地 Docker Compose 测试」）
make dev-hot

# 全量重建（改 Dockerfile / Cargo.toml 依赖 / 非 Rust 改动时才用，10+ 分钟）
make dev-restart

# 日志 / 停止
make dev-logs
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

### gRPC 通信架构
- 使用 **Tonic 0.14** 实现 gRPC 服务端和客户端
- Proto 文件使用 **Protobuf oneof** 实现类型安全的事件系统，完全消除 JSON 序列化
- **GrpcChannelPool** 基于 DashMap 提供高效的连接复用
- **Server Streaming** 用于实时推送进度事件（替代轮询）
- **HTTP 回退机制**：gRPC 失败时自动回退到 HTTP（兼容性保障）
- gRPC 默认端口：`50051`（定义在 `shared_types::GRPC_DEFAULT_PORT`）

**关键文件**：
- `crates/shared_types_grpc/proto/agent.proto` - Proto 定义
- `crates/rcoder/src/grpc/channel_pool.rs` - 连接池
- `crates/rcoder/src/grpc/chat_client.rs` - gRPC 客户端
- `crates/agent_runner/src/grpc/mod.rs` - gRPC 服务实现（`AgentServiceImpl` 定义，方法实现拆分至 `chat.rs`/`subscribe_progress.rs`/`cancel.rs`/`status.rs`/`permission.rs` 等子文件）

### ACP 协议集成
- 使用 `agent-client-protocol = "2"`（官方 SDK），schema 走 `agent_client_protocol::schema::v1`，v1 是稳定 wire 协议
- SDK 全程 `Send`：连接任务用标准 `tokio::spawn` 驱动，**无需 LocalSet / spawn_local**
- 连接模型：`Client.builder().name(...).on_receive_dispatch(...).on_receive_request(...).connect_with(transport, |cx| async {...})`，核心实现见 `crates/agent_abstraction/src/launcher/claude_code_sacp/connection.rs`
- 与所有 ACP v1 agent（Claude Code / nuwaxcode / codex 等）wire 兼容：握手发送 `protocolVersion: 1`

### 并发模型和状态管理
- 使用 **DashMap** 替代 `Arc<RwLock<HashMap>>` 以获得更好的性能
- 使用写时复制 (CoW) 模式进行状态更新
- 主应用使用 `#[tokio::main]`（多线程）
- ACP SDK 完全 Send-safe，连接任务用标准 `tokio::spawn` 驱动，无需 LocalSet

### Docker 容器动态创建
- **多级隔离架构**: 支持三种隔离级别
  - `project` (默认): 每个项目对应一个独立的 Docker 容器
  - `tenant`: 租户级隔离，同一租户共享容器
  - `space`: 空间级隔离，同一空间共享容器
- **容器复用**: 通过 `pod_id` 字段实现跨用户容器复用
- **自动架构检测**: 根据 OS 和 ARCH 自动选择合适的镜像
- **内部网络通信**: 容器间通过 Docker 内部网络直接通信，无需端口映射
- **路径自动解析**: 自动检测容器内路径到宿主机路径的映射

### K8s Pod/PVC 生命周期管理（agent-runner，STS-based）

K8s 模式下 agent-runner（ComputerAgentRunner / WebAgentRunner）的停止流程基于 **StatefulSet**（裸 Pod→STS 改造后，commit d98c923），**PVC 全程保留不删**（数据复用，下次 ensure 重建挂回）：

```
stop_container_by_identifier_inner()  // k8s_agent_pod.rs
  ├─ Step 0: delete_agent_service()         // 删 ClusterIP Service（先摘流量 / DNS）
  ├─ Step 1: delete_agent_statefulset()      // Foreground cascade 删 STS → pod 随之终止（非 scale 0）
  ├─ Step 2: wait_for_pod_terminated()       // 等 pod {sts}-0 完全终止（404），避免与重建 pod 抢 RWO PVC
  └─ Step 3: delete_agent_headless_service() // 删 headless Service（彻底回收）
  —— PVC 保留（日志 "PVC preserved for reuse"），不删——
```

**PVC 保留策略（数据安全硬约束）**：agent 侧 PVC **永不删除**——`stop_container_by_identifier_inner`（k8s_agent_pod.rs）只删 ClusterIP/headless Service + STS + 等 pod 终止，全程不碰 PVC（数据复用，下次 ensure 重建挂回）。`K8sPvcOps` trait（k8s_pvc.rs）提供 `ensure_workspace_pvc` / `resolve_subvolume_path` / `resize_workspace_pvc` / `destroy_workspace_pvc` 等方法；其中 **`destroy_workspace_pvc` 是唯一的 PVC 删除入口，仅 UserApp 经独立 REST 接口 `POST /apps/{id}/storage/destroy` 显式调用**（见 `docs/application-management-service-v2-design.md` §5.4），agent 停止流程不调用。早期 JuiceFS + 裸 Pod 时代的 `delete_workspace_pvc` / `wait_for_pvc_removable` 已在 STS + CephFS + per-agent PVC 改造后移除，由 `destroy_workspace_pvc` 取代。

**关键文件**：
- `crates/docker_manager/src/runtime/k8s_agent_pod.rs` - 核心：`stop_container_by_identifier_inner`（STS 销毁流程）+ `restart_agent_container_inplace`（原地重启）
- `crates/docker_manager/src/runtime/k8s_pod.rs` - Pod 生命周期：`K8sPodOps` trait（wait_for_pod_ready, wait_for_pod_terminated）
- `crates/docker_manager/src/runtime/k8s_pvc.rs` - PVC 生命周期：`K8sPvcOps` trait（ensure_workspace_pvc, resolve_subvolume_path, resize_workspace_pvc, destroy_workspace_pvc）。`destroy_workspace_pvc` 仅 UserApp REST 调用，**agent 侧永不删 PVC**
- `crates/agent_runner/src/shutdown.rs` - 进程优雅关闭：`terminate_children()`（SIGTERM → 3s → SIGKILL）

**Pod 停止时的三道防线**：
1. **preStop lifecycle hook**: `sync && sleep 2`，在 SIGTERM 前 flush 写入 buffer
2. **agent_runner shutdown handler**: 构建进程树，递归收集所有后代 + 孤儿进程（ppid=1），叶子优先 SIGTERM → 等待 3s → SIGKILL
3. **wait_for_pod_terminated**: 等待 Pod 从 API Server 消失（404），超时后 `gracePeriodSeconds=0` 强制删除

### 容器工作空间路径
| 隔离类型 | RCoder 路径 | Computer 路径 |
|---------|------------|--------------|
| project | `/app/project_workspace/{project_id}` | `/app/computer-project-workspace/{user_id}/{project_id}` |
| tenant/space | `/app/project_workspace/{tenant_id}/{space_id}/{project_id}` | `/app/computer-project-workspace/{tenant_id}/{space_id}/{project_id}` |

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
- Rust 1.85+ (2024 Edition, edition 2024 最低要求 1.85)
- Docker 和 Docker Compose
- Claude Code CLI (可选)

## 特殊注意事项

### 禁止事项
1. **禁止使用模拟响应逻辑** - 所有 AI 调用必须真实执行
2. **禁止编写 unsafe 代码** - 项目要求内存安全
3. **ACP schema 类型变更需谨慎** - `shared_types` 直接嵌套 `schema::v1` 类型（StopReason/SessionUpdate/SessionId），升级 SDK 后务必全量编译 + 测试
4. ** Always Response in 中文** - 所有响应必须使用中文

### Docker 容器管理
- **容器名称格式**: `rcoder-agent-{project_id}` 或 `rcoder-agent-{pod_id}` (当 pod_id 提供时)
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

> 💡 **K8s/devspace 下 rcoder 日志写文件、不写 stdout**，`kubectl logs` 几乎为空。详见下文「本地 K8s 测试（devspace + OrbStack）」章节。

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

### 本地 Docker Compose 测试（端口 8090）

用 `make dev-up` 启动 Docker Compose 模式（OrbStack 提供 docker），默认服务端口 `8090`，主容器 `rcoder-rcoder-1`，network `rcoder_default`。

**首次部署**：
```bash
make dev-build      # 构建 Docker 镜像 dev-master-rcoder:latest
make dev-up         # 启动容器
```

**日常开发（推荐：容器内热编译）**：

`docker-compose.yml` 已挂载仓库源码到 `/app/src` + cargo/target 缓存 volume。改 Rust 源码后用 `make dev-hot` 秒级生效，**不用 `make dev-restart` 全量重建（10+ 分钟）**：
```bash
make dev-hot        # 容器内 cargo build --release --bin rcoder + mv 替换 binary + docker restart
                    # 首次较慢（补 cmake/protoc + 全量编译），之后增量秒级（<2min）
```
脚本：`docker/dev-hot-build.sh`。改 Dockerfile / `Cargo.toml` 依赖 / 非 Rust 文件时仍需 `make dev-restart`。

**日志查看**（rcoder 同时写文件 + stdout）：
```bash
make dev-logs       # docker logs（stdout）
# 或查文件（按天滚动、JSON 格式，与 K8s 同款）
docker exec rcoder-rcoder-1 grep -a "ERROR\|\[APP" /app/logs/rcoder.$(date +%Y-%m-%d) | tail -20
```

**端口 / 容器 / 网络**：
- 主服务：`rcoder-rcoder-1`，端口 `8090`
- Pingora 代理：宿主机 `8089` → 容器 `8088`（app HTTP 端口经 `/proxy/{port}` 暴露）
- 网络：`rcoder_default`（动态 app 容器加入，pingora 通过 container_ip 访问）
- app 工作空间：容器 `/app/app-workspace/{app_id}`，宿主机 `docker/app-workspace/`

**app_manager（UserApp）Docker 模式要点**：
- HTTP 端口：Pingora `/proxy/{port}` → container_ip:port；`access.http = http://127.0.0.1:8088/proxy/{port}`（宿主机访问把端口换成 8089）
- TCP 端口：Docker 自动分配 host_port（`access.tcp.node_port`）
- app 容器名：`rcoder-app-{app_id}`，label `managed-by=rcoder-app-manager`
- ⚠️ rcoder 重启后 pingora 内存路由丢失（HTTP 端口需重建 app 才恢复，已知限制）

**Docker Compose vs K8s（devspace）对照**：

| 维度 | Docker Compose | K8s（devspace，详见下文） |
|---|---|---|
| 启动 | `make dev-up` | `devspace dev` |
| 服务端口 | 8090 | 8290 |
| HTTP 暴露 | Pingora `/proxy/{port}` | Envoy Gateway HTTPRoute `/apps/{id}` |
| 热重载 | `make dev-hot`（秒级热编译） | devspace sync（rcoder 不自动重启，需 `kubectl delete pod`） |
| app 计算单元 | 单容器 | Deployment + Pod |
| 日志 | stdout + 文件 | 文件（`kubectl logs` 几乎空） |

### 本地 K8s 测试（devspace + OrbStack）

用 `devspace dev` 启动本地 K8s 环境（OrbStack 提供 k8s），默认服务端口 `8290`，namespace `rcoder-dev`，context `orbstack`。

**Pod / Service 命名规则**：
- rcoder 主服务 Pod：`rcoder-devspace-*`，Service `rcoder-service:8290`
- ComputerAgentRunner Pod：`dev-rcoder-agent-runner-{user_id}`，每个 Pod 配一个 headless Service `{pod-name}-svc`
- WebAgentRunner Pod：`dev-master-rcoder-{project_id}`，同样配 `{pod-name}-svc`
- 容器间 gRPC 地址：`{pod-name}-svc.rcoder-dev.svc.cluster.local:50051`

**⚠️ 日志查看（重要，避免踩坑）**：
- **rcoder 日志写文件、不写 stdout** → `kubectl logs <rcoder-pod>` 几乎为空，别误以为"没执行/没日志"。默认级别就是 debug（`RUST_LOG` 不设也有）。
- 正确方式是查 pod 内日志文件（按天滚动、JSON 格式）：
```bash
RCODER_POD=$(kubectl get pod -n rcoder-dev -o name | grep rcoder-devspace | head -1 | sed 's|pod/||')

# 按关键词查 rcoder 日志
kubectl exec -n rcoder-dev $RCODER_POD -- grep -a "RESOURCE_LIMITS\|CHAT\|ERROR" /app/logs/rcoder.$(date +%Y-%m-%d) | tail -20

# 提取 JSON 行的 timestamp + message 字段（更易读）
kubectl exec -n rcoder-dev $RCODER_POD -- grep -a "RESOURCE_LIMITS" /app/logs/rcoder.$(date +%Y-%m-%d) \
  | python3 -c "import sys,json
for l in sys.stdin:
    d=json.loads(l); print(d.get('timestamp','')[:19], d['fields'].get('message',''))"
```
- **agent_runner Pod 日志在 stdout**，`kubectl logs <agent-runner-pod>` 可直接看。

**测试与验证**：
```bash
# 健康检查
curl http://127.0.0.1:8290/health

# 创建 Pod（不带 resource_limits → 验证 configmap 兜底生效）
curl -X POST http://127.0.0.1:8290/computer/pod/ensure \
  -H 'Content-Type: application/json' \
  -d '{"user_id":"test-u","project_id":"test-p"}'

# 查 Pod resources（K8s 核心：应非空 {cpu,memory}，来自 /app/config.yml 默认值）
kubectl get pod -n rcoder-dev dev-rcoder-agent-runner-test-u \
  -o jsonpath='{.spec.containers[0].resources}{"\n"}'
```

**配置来源**：devspace 部署**无独立 configmap**，`ServiceImageConfig.resource_limits` 等来自镜像内 `/app/config.yml`（ComputerAgentRunner=4GB/2核/8GB swap，WebAgentRunner=2GB/2核/4GB swap）。

**代码热重载**：devspace 会把本地源码 sync 到 pod，但 rcoder 以 `cargo run` 启动、**不自动热重载**——改代码后需重启 rcoder 进程才能生效（devspace 侧重启，或 `kubectl delete pod` 让 Deployment 重建并重新 `cargo run` 编译新代码）。

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
// ACP 连接：Builder + connect_with，标准 tokio::spawn 驱动（无需 LocalSet）
tokio::spawn(async move {
    Client.builder()
        .name("rcoder-agent-runner-sacp")
        .on_receive_dispatch(
            async move |dispatch: Dispatch, _cx: ConnectionTo<Agent>| {
                // 匹配 Dispatch::Notification(message) 等，处理 SessionNotification
                Ok(Handled::Yes)
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .on_receive_request(
            move |req: RequestPermissionRequest, responder: Responder<_>, _cx| async move {
                responder.respond(RequestPermissionResponse::new(...))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, move |cx: ConnectionTo<Agent>| async move {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            cx.send_request(NewSessionRequest::new(cwd)).block_task().await?; // 建会话
            cx.send_request(PromptRequest::new(...)).block_task().await?; // 发 prompt
            Ok(())
        })
        .await
});
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