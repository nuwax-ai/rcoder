# gRPC 通信改造设计文档

## 1. 背景与目标

当前 `rcoder` 与 `agent-runner` (容器内服务) 之间采用 HTTP + SSE (Server-Sent Events) 进行通信。为了提升通信效率、增强类型安全并简化流式数据处理，计划将通信协议升级为 gRPC。

**主要目标：**
1. 使用二进制协议 (Protobuf) 替代 JSON/文本协议，提升性能。
2. 利用 gRPC 的强类型契约，确保服务间接口一致性。
3. 使用 gRPC Server Streaming 替代 SSE，简化实时进度通知的实现。

## 2. 架构变更

### 当前架构 (HTTP/SSE)
- **命令通道**: `rcoder` -> HTTP POST -> `agent-runner` (如 `/chat`, `/agent/stop`)
- **数据通道**: `rcoder` <- SSE Stream <- `agent-runner` (如 `/agent/progress/{session_id}`)
- **缺点**: 需要手动解析 SSE 文本流，类型约束弱。

### 目标架构 (gRPC)
- **统一通道**: `rcoder` -> gRPC -> `agent-runner`
- **协议定义**: 统一在 `shared_types` 模块中定义 `.proto` 文件。
- **端口策略**:
  - `agent-runner` 容器内部暴露 gRPC 端口 (如 `50051`) 用于业务通信。
  - 保留 HTTP 端口 (如 `8086`) 用于 `/health` 检查和监控。

## 3. 接口定义 (Proto)

在 `crates/shared_types/proto/agent.proto` 中定义服务契约。

```protobuf
syntax = "proto3";
package agent;

// Agent 核心服务
service AgentService {
  // 聊天对话接口 (Unary)
  rpc Chat (ChatRequest) returns (ChatResponse);

  // 订阅会话进度流 (Server Streaming)
  // 替代原有的 /agent/progress/{session_id} SSE 接口
  rpc SubscribeProgress (ProgressRequest) returns (stream ProgressEvent);

  // 取消会话任务 (Unary)
  rpc CancelSession (CancelRequest) returns (CancelResponse);
  
  // 获取 Agent 状态 (Unary)
  rpc GetStatus (GetStatusRequest) returns (GetStatusResponse);
}

// === 消息定义 ===

message ChatRequest {
  string project_id = 1;
  string session_id = 2;
  string prompt = 3;
  // 模型配置等可选字段
  optional ModelProviderConfig model_config = 4;
  // 附件列表等...
}

message ChatResponse {
  string request_id = 1;
  bool success = 2;
  // 错误信息等
  optional string error = 3;
}

message ProgressRequest {
  string session_id = 1;
}

message ProgressEvent {
  // 事件类型
  oneof event {
    string log = 1;      // 普通日志
    string thought = 2;  // 思考过程
    string chunk = 3;    // 内容片段
    bool done = 4;       // 完成信号
    string error = 5;    // 错误信息
  }
  // 保留原始 JSON 用于兼容过渡
  string json_payload = 10;
  // 时间戳 (使用 Unix timestamp)
  int64 timestamp = 11;
}

message CancelRequest {
  string session_id = 1;
  string reason = 2;
}

message CancelResponse {
  bool success = 1;
}

message GetStatusRequest {
  string project_id = 1;
}

message GetStatusResponse {
  string status = 1; // "idle", "busy", "error"
}

message ModelProviderConfig {
  string provider = 1;
  string model = 2;
  // ...
}
```

## 4. 模块改造计划

### 4.1. shared_types (公共依赖)
负责 Proto 文件的编译和代码生成。

*   **文件结构**:
    ```
    crates/shared_types/
    ├── Cargo.toml
    ├── build.rs          (新增: 编译 proto)
    ├── proto/            (新增)
    │   └── agent.proto
    └── src/
        ├── lib.rs
        └── grpc/         (新增: 导出生成的代码)
    ```
*   **依赖变更**:
    - `dependencies`: 添加 `tonic`, `prost`
    - `build-dependencies`: 添加 `tonic-build`

### 4.2. agent_runner (Server 端)
负责实现 gRPC 服务端逻辑。

*   **新增模块**: `src/grpc_server.rs`
*   **启动逻辑**: 在 `main.rs` 中，除了启动 Axum HTTP Server (用于 health check)，同时启动 Tonic gRPC Server。
*   **实现细节**:
    - 实现 `AgentService` trait。
    - `SubscribeProgress`: 使用 `tokio::sync::mpsc` 接收内部事件总线的消息，并通过 `ReceiverStream` 转换为 gRPC 流返回。

### 4.3. rcoder (Client 端)
作为 gRPC 客户端调用 `agent-runner`。

*   **连接管理**:
    - 维护 gRPC Channel 连接池 (Tonic 的 Channel 本身是廉价克隆且并发安全的)。
    - 根据 `project_id` -> `container_ip` 动态构建 Endpoint。
*   **SSE 转发改造**:
    - 在 `handler/agent_session_notification.rs` 中，不再使用 `reqwest` 读取 SSE 流。
    - 改为调用 gRPC `SubscribeProgress` 接口。
    - 将接收到的 `ProgressEvent` gRPC 消息转换为 Axum SSE `Event` 并 yield 给前端。

## 5. 实施步骤

1.  **基础依赖引入**: 修改 `shared_types` 的 `Cargo.toml`，引入 `tonic`, `prost` 等。
2.  **Proto 定义与编译**: 创建 `agent.proto` 和 `build.rs`，确保 `cargo build` 能成功生成 Rust 代码。
3.  **Server 端实现**: 在 `agent_runner` 中实现 gRPC 服务，并支持双协议启动。
4.  **Client 端联调**: 在 `rcoder` 中编写测试路由，验证 gRPC 调用通畅。
5.  **业务迁移**:
    - 迁移 `/agent/progress` 接口 (SSE 转发)。
    - 迁移 `/chat` 接口。
    - 迁移 `/agent/cancel` 接口。
6.  **清理**: 移除 `agent-runner` 中旧的业务 HTTP 接口代码，仅保留 Health Check。

