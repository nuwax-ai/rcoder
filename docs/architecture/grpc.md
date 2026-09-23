# gRPC 内部通信

rcoder 主服务与 agent_runner 之间的内部通信使用 gRPC（Tonic），对外 HTTP API 完全不变——外部客户端只面对 HTTP/SSE。

## 通信链路

```
┌─────────────────┐
│  外部客户端      │
│  (HTTP/SSE)     │
└────────┬────────┘
         │ HTTP POST /chat
         ▼
┌─────────────────────────────────┐
│  RCoder (http-server)           │
│  chat 转发 → gRPC 请求           │
│         │ GrpcChannelPool       │
└─────────┼───────────────────────┘
          │ gRPC Chat（Protobuf）
          ▼
┌─────────────────────────────────┐
│  Agent Runner（容器/Pod 内）      │
│  AgentServiceImpl               │
│  - Chat / CancelSession / …     │
│  - SubscribeProgress            │
│    (Server Streaming)           │
└─────────┬───────────────────────┘
          │ gRPC ProgressEvent 流
          ▼
┌─────────────────────────────────┐
│  RCoder (SSE 桥接)               │
│  gRPC 流 → SSE 事件              │
└─────────┬───────────────────────┘
          │ SSE
          ▼
     外部客户端
```

## 服务定义

proto 权威定义在 [`crates/shared_types_grpc/proto/agent.proto`](../../crates/shared_types_grpc/proto/agent.proto)，服务名为 `AgentService`：

| 方法 | 类型 | 说明 |
|------|------|------|
| `Chat` | Unary | 发送聊天请求 |
| `SubscribeProgress` | Server Streaming | 订阅进度事件流 |
| `CancelSession` | Unary | 取消会话任务 |
| `ResolvePermission` | Unary | 权限请求裁决 |
| `GetStatus` | Unary | 查询 Agent 状态 |
| `StopAgent` | Unary | 停止 Agent |
| `GetContainerStatus` / `GetVncStatus` | Unary | 容器 / VNC 状态 |
| `ListAgents` / `GetAgent` / `CheckAgent` | Unary | Agent 清单与探测 |
| `InstallAgent`（客户端流式）/ `UninstallAgent` | Streaming/Unary | Agent 安装/卸载 |

实现位置：

- 服务端：`crates/agent_runner/src/grpc/`（`chat.rs`、`subscribe_progress.rs`、`cancel.rs`、`status.rs`、`stop_agent.rs`、`permission.rs`、`vnc_probe.rs` 等）
- 客户端：`crates/rcoder-engine/src/grpc/`（`chat_client.rs`、`sse_stream.rs`、`channel_pool.rs`、`status_query.rs`、`retry.rs`）
- 容器内服务地址：`{容器地址}:50051`（K8s 下为 `{pod}-svc.{ns}.svc.cluster.local:50051`）

## 进度事件：Protobuf oneof

`ProgressEvent` 使用 `oneof` 承载 8 种事件类型，编译期类型检查、二进制编码，避免"事件类型字符串 + JSON payload"的二次解析：

```protobuf
message ProgressEvent {
  oneof event {
    LogEvent log = 1;
    ThinkingEvent thinking = 2;
    ChunkEvent chunk = 3;
    CompletionEvent completion = 4;
    ErrorEvent error = 5;
    AskConfirmationEvent ask_confirmation = 6;
    ProgressNotificationEvent progress_notification = 7;
    ToolUseEvent tool_use = 8;
  }
  int64 timestamp = 11;
}
```

事件在三层之间的转换路径：

```
UnifiedSessionMessage（内部统一消息类型）
    ↓ agent_runner 侧转换
ProgressEvent（gRPC Protobuf）
    ↓ rcoder-engine 侧还原为统一消息
UnifiedSessionMessage
    ↓ SSE 桥接
SSE Event（外部 HTTP SSE）
```

SSE 桥接以统一消息的**子类型（sub_type）作为事件名**（如 `agent_message_chunk`、`tool_call`），前端按 sub_type 监听；事件信封结构与断线续传见[会话与 SSE](../concepts/agent-sessions.md)。

## 连接池（GrpcChannelPool）

rcoder 侧以容器地址为键缓存 gRPC `Channel`（HTTP/2 连接复用），避免每请求重建连接；带连接/请求超时配置，支持失效连接移除。实现：`crates/rcoder-engine/src/grpc/channel_pool.rs`。

## 可观测性

gRPC 关键 span 的耗时直方图（`grpc_request_duration_seconds{method="chat"|"dial"}`）与调用计数（`grpc_requests_total`）经 `/metrics` 暴露；跨服务链路追踪见[可观测性指南](../observability.md)——rcoder → agent_runner 的 gRPC 请求统一注入 W3C `traceparent`，两侧日志 JSON 顶层共享同一 `trace_id`。

## 设计收益

- **类型安全**：Protobuf 编译期检查，消除 JSON 解析错误
- **性能**：二进制序列化替代 JSON，消息更小更快
- **连接复用**：全局连接池 + HTTP/2 多路复用
- **实时性**：Server Streaming 替代轮询推送进度
- **兼容**：对外 HTTP API 不变，gRPC 仅用于内部通信
