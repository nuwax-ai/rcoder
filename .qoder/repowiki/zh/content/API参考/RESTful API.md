# RESTful API

<cite>
**本文档引用文件**  
- [router.rs](file://crates/rcoder/src/router.rs#L1-L203)
- [health_handler.rs](file://crates/rcoder/src/handler/health_handler.rs#L1-L36)
- [agent_session_notification.rs](file://crates/rcoder/src/handler/agent_session_notification.rs#L1-L439)
- [chat_handler.rs](file://crates/rcoder/src/handler/chat_handler.rs#L1-L232)
- [agent_cancel_handler.rs](file://crates/rcoder/src/handler/agent_cancel_handler.rs#L1-L169)
- [agent_stop_handler.rs](file://crates/rcoder/src/handler/agent_stop_handler.rs#L1-L266)
- [proxy_api.rs](file://crates/rcoder/src/handler/proxy_api.rs#L1-L195)
- [agent_session_notify.rs](file://crates/rcoder/src/model/agent_session_notify.rs#L1-L378)
- [http_result.rs](file://crates/rcoder/src/model/http_result.rs#L1-L103)
- [app_error.rs](file://crates/rcoder/src/model/app_error.rs#L1-L26)
- [attachment.rs](file://crates/rcoder/src/model/attachment.rs#L1-L216)
</cite>

## 目录
1. [简介](#简介)
2. [API概览](#api概览)
3. [健康检查接口](#健康检查接口)
4. [聊天接口](#聊天接口)
5. [会话通知接口](#会话通知接口)
6. [任务取消接口](#任务取消接口)
7. [Agent停止接口](#agent停止接口)
8. [Agent状态查询接口](#agent状态查询接口)
9. [代理接口](#代理接口)
10. [错误处理机制](#错误处理机制)
11. [认证与限流](#认证与限流)
12. [中间件链](#中间件链)
13. [客户端最佳实践](#客户端最佳实践)

## 简介
RCoder AI 服务提供基于 ACP (Agent Client Protocol) 的完整 AI 代理集成解决方案。本 API 文档详细描述了所有 RESTful 端点，包括同步请求响应式 API 和基于 Server-Sent Events (SSE) 的实时通知接口。系统支持智能对话、实时进度推送、会话管理、项目隔离和高性能反向代理等功能。

**API 基础信息**
- **协议**: HTTP/HTTPS
- **数据格式**: JSON
- **字符编码**: UTF-8
- **内容类型**: `application/json`
- **SSE 内容类型**: `text/event-stream`

**环境配置**
- **本地开发**: `http://localhost:3000`
- **生产环境**: `https://api.rcoder.com`
- **测试环境**: `https://staging-api.rcoder.com`

## API概览
RCoder API 提供了多个功能模块，通过不同的端点进行访问。所有 API 响应都遵循统一的响应格式，包含 `code`、`message`、`data` 和 `tid`（追踪ID）字段。

```mermaid
graph TD
A[API入口] --> B[系统健康检查]
A --> C[AI聊天对话]
A --> D[Agent会话管理]
A --> E[反向代理服务]
B --> B1[/health]
C --> C1[/chat]
D --> D1[/agent/progress/{session_id}]
D --> D2[/agent/session/cancel]
D --> D3[/agent/stop]
D --> D4[/agent/status/{project_id}]
E --> E1[/proxy/status]
E --> E2[/proxy/stats]
E --> E3[/proxy/config]
E --> E4[/proxy/{port}]
E --> E5[/proxy/{port}/{*path}]
```

**图示来源**
- [router.rs](file://crates/rcoder/src/router.rs#L1-L203)

## 健康检查接口
健康检查接口用于验证服务的运行状态，是系统可用性监控的基础。

### 端点信息
- **HTTP方法**: GET
- **URL路径**: `/health`
- **标签**: system
- **操作ID**: health_check

### 请求参数
无请求参数。

### 响应格式
成功响应返回 `200 OK` 状态码和健康状态信息。

#### 成功响应 (200)
```json
{
  "success": true,
  "data": {
    "status": "healthy",
    "timestamp": "2023-12-01T10:30:00Z",
    "service": "rcoder-ai-service"
  },
  "error": null,
  "tid": "abc123def456"
}
```

**字段说明**
- `status`: 服务状态，固定为 "healthy"
- `timestamp`: 当前时间戳，UTC 格式
- `service`: 服务名称

#### cURL 示例
```bash
curl -X GET "http://localhost:3000/health" \
     -H "Content-Type: application/json"
```

**接口来源**
- [health_handler.rs](file://crates/rcoder/src/handler/health_handler.rs#L1-L36)

## 聊天接口
聊天接口是核心功能，用于发送用户请求并启动 AI 代理处理。

### 端点信息
- **HTTP方法**: POST
- **URL路径**: `/chat`
- **标签**: chat
- **操作ID**: handle_chat

### 请求参数
#### 请求头
- `Content-Type`: `application/json` (必需)

#### 请求体结构
```json
{
  "prompt": "帮我写一个Rust的Hello World程序",
  "project_id": "test_project",
  "session_id": "session456",
  "attachments": [],
  "data_source_attachments": [],
  "model_provider": {
    "id": "openai_gpt4",
    "name": "openai",
    "base_url": "https://api.openai.com/v1",
    "api_key": "sk-...",
    "requires_openai_auth": true,
    "default_model": "gpt-4",
    "api_protocol": "openai"
  },
  "request_id": "req_123456789"
}
```

**字段说明**
- `prompt`: 用户输入的提示文本（必需）
- `project_id`: 项目ID，用于隔离工作空间（可选）
- `session_id`: 会话ID，用于关联对话（可选）
- `attachments`: 附件列表，支持文本、图像、音频、文档等多媒体内容
- `data_source_attachments`: 数据源附件列表，JSON 字符串数组
- `model_provider`: 模型提供商配置
- `request_id`: 请求ID，用于追踪和重试（可选）

#### 附件结构
附件支持多种类型，通过 `type` 字段区分：

**文本附件**
```json
{
  "type": "Text",
  "content": {
    "id": "uuid123",
    "source": {
      "source_type": "FilePath",
      "data": { "path": "src/main.rs" }
    },
    "filename": "main.rs",
    "description": "主程序文件"
  }
}
```

**图像附件**
```json
{
  "type": "Image",
  "content": {
    "id": "uuid456",
    "source": {
      "source_type": "Base64",
      "data": { "data": "base64encoded", "mime_type": "image/jpeg" }
    },
    "mime_type": "image/jpeg",
    "filename": "screenshot.jpg",
    "dimensions": { "width": 800, "height": 600 }
  }
}
```

### 响应格式
#### 成功响应 (200)
```json
{
  "success": true,
  "data": {
    "project_id": "test_project",
    "session_id": "session456",
    "error": null
  },
  "error": null,
  "tid": "abc123def456"
}
```

#### 错误响应
- **400 Bad Request**: 请求参数错误
- **500 Internal Server Error**: 服务器内部错误

#### cURL 示例
```bash
curl -X POST "http://localhost:3000/chat" \
     -H "Content-Type: application/json" \
     -d '{
  "prompt": "帮我写一个Rust的Hello World程序",
  "project_id": "test_project",
  "model_provider": {
    "id": "openai_gpt4",
    "name": "openai",
    "base_url": "https://api.openai.com/v1",
    "api_key": "sk-...",
    "requires_openai_auth": true,
    "default_model": "gpt-4",
    "api_protocol": "openai"
  }
}'
```

**接口来源**
- [chat_handler.rs](file://crates/rcoder/src/handler/chat_handler.rs#L1-L232)
- [attachment.rs](file://crates/rcoder/src/model/attachment.rs#L1-L216)

## 会话通知接口
会话通知接口通过 Server-Sent Events (SSE) 协议实时推送 AI 代理执行进度和状态更新。

### 端点信息
- **HTTP方法**: GET
- **URL路径**: `/agent/progress/{session_id}`
- **标签**: agent
- **操作ID**: agent_session_notification
- **内容类型**: `text/event-stream`

### 请求参数
#### 路径参数
- `session_id`: 会话ID，用于标识特定的会话连接

#### 查询参数
无查询参数。

### 响应格式
建立 SSE 连接后，服务器会持续推送事件流，直到连接关闭。

#### SSE 事件格式
```
event: [事件类型]
data: [JSON格式的消息]
```

#### 事件类型映射
| 事件类型 | 对应消息类型 | 说明 |
|---------|------------|------|
| `prompt_start` | SessionPromptStart | 用户发送prompt开始 |
| `prompt_end` | SessionPromptEnd | Agent执行结束 |
| `user_message_chunk` | AgentSessionUpdate | 用户消息块 |
| `agent_message_chunk` | AgentSessionUpdate | Agent响应消息块 |
| `agent_thought_chunk` | AgentSessionUpdate | Agent思考过程 |
| `tool_call` | AgentSessionUpdate | 工具调用通知 |
| `tool_call_update` | AgentSessionUpdate | 工具调用状态更新 |
| `available_commands_update` | AgentSessionUpdate | 可用命令更新 |
| `current_mode_update` | AgentSessionUpdate | 当前模式更新 |
| `heartbeat` | Heartbeat | 心跳消息 |

#### 统一消息结构
所有消息都遵循 `UnifiedSessionMessage` 结构：

```json
{
  "session_id": "session456",
  "message_type": "SessionPromptStart",
  "sub_type": "prompt_start",
  "data": {},
  "timestamp": "2023-12-01T10:30:00Z"
}
```

**字段说明**
- `session_id`: 会话ID
- `message_type`: 消息主类型
- `sub_type`: 消息子类型
- `data`: 具体数据内容
- `timestamp`: 消息时间戳

#### 典型场景示例
**用户请求开始 (SessionPromptStart)**
```json
{
  "session_id": "session456",
  "message_type": "SessionPromptStart",
  "sub_type": "prompt_start",
  "data": {
    "type": "prompt_start",
    "prompt": "帮我写一个Rust的Hello World程序",
    "attachments": [
      {
        "type": "text",
        "content": "这是附加的代码要求"
      }
    ],
    "user_id": "user123",
    "project_id": "test_project"
  },
  "timestamp": "2023-12-01T10:30:00Z"
}
```

**Agent思考过程 (AgentThoughtChunk)**
```json
{
  "session_id": "session456",
  "message_type": "AgentSessionUpdate",
  "sub_type": "agent_thought_chunk",
  "data": {
    "content": {
      "type": "text",
      "text": "用户要求写一个Hello World程序，我需要创建main.rs文件并包含基本的println!宏调用。",
      "annotations": null,
      "meta": null
    },
    "confidence": 0.95
  },
  "timestamp": "2023-12-01T10:30:01Z"
}
```

**执行结束 (SessionPromptEnd)**
```json
{
  "session_id": "session456",
  "message_type": "SessionPromptEnd",
  "sub_type": "end_turn",
  "data": {
    "stop_reason": "end_turn",
    "message": "成功创建了Hello World程序",
    "tool_calls": [
      {
        "name": "write_file",
        "status": "completed",
        "duration_ms": 150
      }
    ],
    "total_tokens": 245,
    "duration_ms": 3200
  },
  "timestamp": "2023-12-01T10:30:05Z"
}
```

#### cURL 示例
```bash
curl -X GET "http://localhost:3000/agent/progress/session456" \
     -H "Accept: text/event-stream"
```

**接口来源**
- [agent_session_notification.rs](file://crates/rcoder/src/handler/agent_session_notification.rs#L1-L439)
- [agent_session_notify.rs](file://crates/rcoder/src/model/agent_session_notify.rs#L1-L378)

## 任务取消接口
任务取消接口用于取消正在执行的 AI 代理任务。

### 端点信息
- **HTTP方法**: POST
- **URL路径**: `/agent/session/cancel`
- **标签**: agent
- **操作ID**: agent_session_cancel

### 请求参数
#### 查询参数
- `project_id`: 项目ID，用于标识特定的项目
- `session_id`: 会话ID，用于标识要取消的会话

### 响应格式
#### 成功响应 (200)
```json
{
  "success": true,
  "data": {
    "success": true,
    "session_id": "session456"
  },
  "error": null,
  "tid": "abc123def456"
}
```

#### 错误响应
- **400 Bad Request**: 请求参数错误
- **404 Not Found**: 未找到对应的项目或会话
- **500 Internal Server Error**: 取消操作失败

#### cURL 示例
```bash
curl -X POST "http://localhost:3000/agent/session/cancel?project_id=test_project&session_id=session456" \
     -H "Content-Type: application/json"
```

**接口来源**
- [agent_cancel_handler.rs](file://crates/rcoder/src/handler/agent_cancel_handler.rs#L1-L169)

## Agent停止接口
Agent停止接口用于停止指定项目的 Agent 服务并清理相关资源。

### 端点信息
- **HTTP方法**: POST
- **URL路径**: `/agent/stop`
- **标签**: agent
- **操作ID**: agent_stop

### 请求参数
#### 查询参数
- `project_id`: 项目ID

### 响应格式
#### 成功响应 (200)
```json
{
  "success": true,
  "data": {
    "success": true,
    "project_id": "test_project",
    "session_id": "session123",
    "message": "Agent服务已成功停止，所有资源已清理"
  },
  "error": null,
  "tid": "abc123def456"
}
```

#### 错误响应
- **400 Bad Request**: 请求参数错误
- **404 Not Found**: 未找到对应的 Agent 服务

#### cURL 示例
```bash
curl -X POST "http://localhost:3000/agent/stop?project_id=test_project" \
     -H "Content-Type: application/json"
```

**接口来源**
- [agent_stop_handler.rs](file://crates/rcoder/src/handler/agent_stop_handler.rs#L1-L266)

## Agent状态查询接口
Agent状态查询接口用于获取指定项目的 Agent 服务状态信息。

### 端点信息
- **HTTP方法**: GET
- **URL路径**: `/agent/status/{project_id}`
- **标签**: agent
- **操作ID**: agent_status

### 请求参数
#### 路径参数
- `project_id`: 项目ID

### 响应格式
#### 成功响应 (200)
**Agent存活**
```json
{
  "success": true,
  "data": {
    "project_id": "test_project",
    "is_alive": true,
    "session_id": "session123",
    "status": "Active",
    "last_activity": "2024-01-01T12:00:00Z",
    "created_at": "2024-01-01T10:00:00Z",
    "model_provider": {
      "id": "custom",
      "name": "custom",
      "api_protocol": "OpenAI",
      "default_model": "gpt-4"
    }
  },
  "error": null,
  "tid": "abc123def456"
}
```

**Agent不存活**
```json
{
  "success": true,
  "data": {
    "project_id": "test_project",
    "is_alive": false
  },
  "error": null,
  "tid": "abc123def456"
}
```

#### 错误响应
- **400 Bad Request**: 请求参数错误

#### cURL 示例
```bash
curl -X GET "http://localhost:3000/agent/status/test_project" \
     -H "Content-Type: application/json"
```

**接口来源**
- [agent_stop_handler.rs](file://crates/rcoder/src/handler/agent_stop_handler.rs#L1-L266)

## 代理接口
代理接口提供对 Pingora 反向代理服务的访问和管理功能。

### 端点信息
- **标签**: proxy

### 支持的端点
| HTTP方法 | URL路径 | 功能 |
|--------|-------|------|
| GET | `/proxy/status` | 查看代理服务状态 |
| GET | `/proxy/stats` | 查看代理统计信息 |
| GET | `/proxy/config` | 查看代理配置 |
| GET | `/proxy/{port}` | 代理到指定端口 |
| GET | `/proxy/{port}/{*path}` | 代理到指定端口和路径 |

### 响应格式
#### 代理状态 (ProxyStatus)
```json
{
  "status": "running",
  "listen_port": 8080,
  "default_backend_port": 3000,
  "default_backend_host": "127.0.0.1",
  "backends": [
    {
      "port": 3000,
      "host": "127.0.0.1",
      "health_status": "healthy",
      "last_check": "2025-01-12T10:30:00Z"
    }
  ],
  "load_balancer": {
    "algorithm": "round-robin",
    "health_check_enabled": true,
    "backend_count": 3
  }
}
```

#### 代理统计 (ProxyStats)
```json
{
  "total_requests": 15420,
  "successful_requests": 15200,
  "failed_requests": 220,
  "avg_response_time_ms": 35.5,
  "active_connections": 12,
  "port_stats": [
    {
      "port": 3000,
      "requests": 8560,
      "success_rate": 0.987,
      "avg_response_time_ms": 28.3
    }
  ]
}
```

#### cURL 示例
```bash
# 查看代理状态
curl -X GET "http://localhost:3000/proxy/status"

# 代理到端口3000
curl -X GET "http://localhost:3000/proxy/3000/api/users"
```

**接口来源**
- [proxy_api.rs](file://crates/rcoder/src/handler/proxy_api.rs#L1-L195)

## 错误处理机制
RCoder API 采用统一的错误处理机制，确保客户端能够一致地处理各种错误情况。

### 统一响应格式
所有 API 响应都遵循 `HttpResult<T>` 结构：

```json
{
  "code": "0000",
  "message": "成功",
  "data": {},
  "tid": "abc123def456",
  "success": true
}
```

**字段说明**
- `code`: 错误码，"0000" 表示成功
- `message`: 人类可读的错误消息
- `data`: 响应数据，成功时包含数据，失败时为 null
- `tid`: 追踪ID，用于日志关联
- `success`: 布尔值，表示操作是否成功

### 标准错误码
| HTTP状态码 | 错误码 | 说明 |
|----------|-------|------|
| 400 | INVALID_PARAMS | 请求参数错误 |
| 400 | VALIDATION001 | 请求参数验证失败 |
| 404 | SESSION_NOT_FOUND | 会话不存在 |
| 404 | PROJECT_NOT_FOUND | 项目不存在 |
| 500 | INTERNAL001 | 服务器内部错误 |
| 500 | LOCAL001 | 本地任务发送错误 |
| 500 | CANCEL_FAILED | 取消操作失败 |

### 错误响应示例
```json
{
  "code": "INVALID_PARAMS",
  "message": "Invalid project_id or session_id",
  "data": null,
  "tid": "abc123def456",
  "success": false
}
```

**接口来源**
- [http_result.rs](file://crates/rcoder/src/model/http_result.rs#L1-L103)
- [app_error.rs](file://crates/rcoder/src/model/app_error.rs#L1-L26)

## 认证与限流
RCoder API 目前未实现统一的认证机制，但通过项目ID和会话ID实现基本的访问控制。

### 认证方式
- **API Key**: 通过 `model_provider.api_key` 字段传递
- **JWT**: 暂未实现
- **项目隔离**: 通过 `project_id` 实现多租户隔离

### 调用频率限制
- **默认策略**: 无显式限流
- **建议**: 客户端应实现合理的重试逻辑，避免过度请求
- **SSE连接**: 建议设置合理的超时和重连机制

## 中间件链
RCoder API 使用 Axum 框架的中间件机制，为所有端点提供统一的功能支持。

### 中间件组成
- **Tracing**: 请求追踪和日志记录
- **SSE流处理**: Server-Sent Events 支持
- **错误处理**: 统一错误响应格式化
- **状态管理**: 共享应用状态

### 路由注册逻辑
所有端点在 `router.rs` 中通过 `create_router` 函数注册：

```rust
let api_routes = Router::new()
    .route("/health", get(handler::health_check))
    .route("/chat", post(handler::handle_chat))
    .route("/agent/progress/{session_id}", get(handler::agent_session_notification))
    // ... 其他路由
    .with_state(state.clone());
```

**接口来源**
- [router.rs](file://crates/rcoder/src/router.rs#L1-L203)

## 客户端最佳实践
为确保稳定可靠的 API 使用体验，建议遵循以下最佳实践。

### 重试逻辑建议
1. **指数退避**: 失败后等待 1s、2s、4s、8s 后重试
2. **最大重试次数**: 建议 3-5 次
3. **错误类型判断**: 仅对 5xx 错误和网络错误进行重试
4. **幂等性保证**: 确保重试不会产生副作用

### 超时配置
| 操作类型 | 建议超时 |
|--------|--------|
| 同步请求 | 30秒 |
| SSE连接 | 5分钟 |
| 心跳间隔 | 30秒 |

### SSE连接管理
1. **自动重连**: 断开后立即重试，指数退避
2. **心跳检测**: 监听 `heartbeat` 事件确保连接活跃
3. **消息队列**: 缓冲消息避免UI阻塞
4. **连接清理**: 不再需要时主动关闭连接

### 错误处理
1. **分类处理**: 区分客户端错误和服务器错误
2. **用户提示**: 提供有意义的错误信息
3. **日志记录**: 记录错误详情用于调试
4. **降级策略**: 服务不可用时提供基本功能