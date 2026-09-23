# Agent 会话与 SSE 进度流

本文说明 RCoder 的 AI 会话主链：如何发起对话、如何消费实时进度流（SSE）、事件结构与断线续传机制。接口的完整参数以运行时 OpenAPI 文档为准（`/api/docs`）。

## 会话主链

```
客户端 POST /chat（prompt + project_id）
    ↓ HTTP
rcoder（确保容器/Pod 就绪 → gRPC Chat）
    ↓ gRPC
agent_runner（驱动 ACP agent：Claude Code / Codex …）
    ↓ Server Streaming（进度事件）
rcoder（转 SSE）
    ↓ SSE
客户端 GET /agent/progress/{session_id}
```

发起 chat 后即可订阅进度流；流式事件实时反映 agent 的思考、工具调用与产出。

## SSE 事件信封

每条 SSE 消息的 `event` 名是子类型（`subType`），`data` 是统一消息信封的 JSON（camelCase）：

```json
{
  "sessionId": "…",
  "messageType": "agentSessionUpdate",
  "subType": "agent_message_chunk",
  "data": { … },
  "timestamp": "…"
}
```

主类型（`messageType`）共五种：

| messageType | 含义 |
|---|---|
| `sessionPromptStart` | 用户发送 prompt，回合开始 |
| `agentSessionUpdate` | 执行过程更新（正文片段/思考/工具调用等） |
| `sessionPromptEnd` | 回合结束（end_turn / cancelled / error） |
| `acpRequestPermission` | 权限审批请求（见[权限审批](permissions.md)） |
| `heartbeat` | 连接保活 |

常见子类型（`subType`，即 SSE `event` 名）：

- 正文与思考：`agent_message_chunk`、`agent_thought_chunk`
- 工具：`tool_call`、`tool_call_update`
- 回合结束：`end_turn`、`error`、`cancelled`、`refusal`、`stream_ended`（rcoder 合成的关流信号）
- 状态同步：`usage_update`、`session_info_update`、`available_commands_update`、`current_mode_update`、`config_option_update`、`max_turn_requests`
- 保活：`ping`

回合终态（end_turn / error / stream_ended）后服务端关闭流；`cancelled` 是正常终止而非连接错误。

## 断线续传

进度流自带顺序号与重放机制：

- 每条 SSE 消息带 `id:<seq>`，从 0 递增。
- 客户端断线重连时携带 `Last-Event-ID` 请求头，服务端从该序号之后增量重放，不丢事件、不重复。
- agent 侧重启会重置事件流（`cursor_reset`/`StreamReset` 类事件），客户端应整体重建展示状态。
- 回合终态关流后重连不会得到新事件；新一轮对话需要新的订阅。

## 会话控制

| 端点 | 说明 |
|---|---|
| `POST /chat` | 发起会话回合 |
| `GET /agent/progress/{session_id}` | SSE 进度流 |
| `POST /agent/session/cancel` | 取消执行中的会话任务 |
| `POST /agent/stop` | 停止 agent（不销毁容器） |
| `GET /agent/status/{project_id}` | 查询 agent 状态 |

## Computer Agent 会话

Computer Agent（容器化代理环境，带 VNC 桌面/音频/IME）的会话链同构，端点族在 `/computer/*`：`POST /computer/chat`、`GET /computer/progress/{session_id}`、`/computer/agent/stop|status|session/cancel`。另有开发态 `/devcomputer/*` 家族。事件信封与续传机制与主链完全一致。

## Agent 工作目录

每个 agent 会话绑定一个工作目录（`work_dir`）：

- **容器模式**（默认）：工作目录位于 agent 容器内的项目工作区。
- **宿主机模式**（deploy-host）：工作目录直接映射到宿主机路径。

该目录承载 agent 读写的项目文件；文件级操作（浏览/上传/版本备份等）由[文件服务](file-services.md)提供。

## 相关文档

- [gRPC 内部通信](../architecture/grpc.md)：chat 主链的 gRPC 细节
- [权限审批](permissions.md)：`acpRequestPermission` 事件与裁决回执
- [可观测性指南](../observability.md)：按 trace_id 串联请求日志
