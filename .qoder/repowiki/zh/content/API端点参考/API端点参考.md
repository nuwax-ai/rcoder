# API端点参考

<cite>
**本文档引用的文件**   
- [router.rs](file://crates/rcoder/src/router.rs)
- [chat_handler.rs](file://crates/rcoder/src/handler/chat_handler.rs)
- [health_handler.rs](file://crates/rcoder/src/handler/health_handler.rs)
- [agent_session_notification.rs](file://crates/rcoder/src/handler/agent_session_notification.rs)
- [agent_cancel_handler.rs](file://crates/rcoder/src/handler/agent_cancel_handler.rs)
- [agent_stop_handler.rs](file://crates/rcoder/src/handler/agent_stop_handler.rs)
- [proxy_api.rs](file://crates/rcoder/src/handler/proxy_api.rs)
- [proxy_handler_api.rs](file://crates/rcoder/src/handler/proxy_handler_api.rs)
- [http_result.rs](file://crates/shared_types/src/model/http_result.rs)
- [app_error.rs](file://crates/shared_types/src/model/app_error.rs)
- [http_test.rest](file://http_test.rest)
- [README.md](file://README.md)
</cite>

## 目录
1. [简介](#简介)
2. [API概览](#api概览)
3. [核心API端点](#核心api端点)
4. [Pingora反向代理API](#pingora反向代理api)
5. [请求/响应格式](#请求响应格式)
6. [认证方法](#认证方法)
7. [错误处理策略](#错误处理策略)
8. [安全考虑](#安全考虑)
9. [速率限制](#速率限制)
10. [版本信息](#版本信息)
11. [客户端实现指南](#客户端实现指南)
12. [性能优化技巧](#性能优化技巧)
13. [调试工具与监控方法](#调试工具与监控方法)

## 简介
RCoder是一个基于Rust构建的现代化AI驱动开发平台，通过ACP（Agent Client Protocol）协议实现与多种AI代理的统一交互。本API文档详细描述了RCoder提供的RESTful API接口，包括HTTP方法、URL模式、请求/响应模式、认证方法等关键信息。平台提供简洁的HTTP API接口，让开发者能够轻松集成和管理AI辅助开发功能。

**API端点**
- `/health`：健康检查
- `/chat`：发送聊天消息给AI代理
- `/agent/progress/{session_id}`：获取统一实时进度流
- `/agent/session/cancel`：取消正在执行的任务
- `/agent/stop`：停止当前Agent
- `/agent/status/{project_id}`：查询Agent状态
- `/api/docs`：Swagger UI API文档
- `/proxy/status`、`/proxy/config`、`/proxy/stats`：Pingora代理状态查询接口

## API概览
RCoder API基于Axum框架构建，提供现代化的REST API与统一的SSE（Server-Sent Events）进度流。API设计遵循RESTful原则，使用标准的HTTP方法和状态码。所有API响应都遵循统一的`HttpResult<T>`格式，包含`code`、`message`、`data`和`success`字段。

```mermaid
graph TB
A[客户端] --> B[Axum HTTP服务器]
A --> C[Pingora代理]
B --> D[API路由]
B --> E[代理工作器 (LocalSet)]
C --> F[后端: 127.0.0.1:{端口}]
```

- Axum主服务负责业务API、会话管理与SSE进度流
- Pingora独立监听代理端口，按路径前缀`/proxy/{port}/{path}`转发到指定后端
- 两者并行运行，互不阻塞；Axum中的`/proxy/...`路由仅作为文档与重定向到Pingora

## 核心API端点

### 健康检查
健康检查端点用于验证服务的运行状态。

**端点信息**
- **URL**: `/health`
- **方法**: `GET`
- **标签**: `system`
- **描述**: 检查服务的健康状态

**成功响应示例**
```json
{
  "status": "healthy",
  "timestamp": "2024-01-01T00:00:00Z",
  "service": "rcoder-ai-service"
}
```

**响应状态码**
- `200`: 服务健康

**Section sources**
- [health_handler.rs](file://crates/rcoder/src/handler/health_handler.rs#L1-L36)

### 聊天接口
聊天接口用于向AI代理发送消息并启动会话。

**端点信息**
- **URL**: `/chat`
- **方法**: `POST`
- **标签**: `chat`
- **请求体类型**: `application/json`

**请求参数**
| 参数 | 类型 | 必需 | 描述 | 示例 |
|------|------|------|------|------|
| `prompt` | string | 是 | 用户输入的提示 | "帮我写一个Rust的Hello World程序" |
| `project_id` | string | 否 | 可选的项目ID | "test_project" |
| `session_id` | string | 否 | 可选的会话ID，不提供则创建新会话 | "session456" |
| `attachments` | array | 否 | 可选的附件列表 | [] |
| `data_source_attachments` | array | 否 | 数据源附件列表 | [] |
| `model_provider` | object | 否 | 模型配置 | 见示例 |
| `request_id` | string | 否 | 可选的请求ID，用于追踪 | "req_123456789" |

**请求体示例**
```json
{
  "prompt": "帮我写一个Rust的Web API项目",
  "project_id": "my-project",
  "session_id": "session123",
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

**成功响应示例**
```json
{
  "success": true,
  "data": {
    "project_id": "test_project",
    "session_id": "session456",
    "error": null,
    "request_id": "req_123456789"
  },
  "error": null
}
```

**响应状态码**
- `200`: 成功处理聊天请求
- `500`: 服务器内部错误或容器服务异常

**Section sources**
- [chat_handler.rs](file://crates/rcoder/src/handler/chat_handler.rs#L1-L431)

### 实时进度流
通过SSE（Server-Sent Events）协议获取AI代理执行进度的实时推送。

**端点信息**
- **URL**: `/agent/progress/{session_id}`
- **方法**: `GET`
- **标签**: `agent`
- **内容类型**: `text/event-stream`

**路径参数**
| 参数 | 类型 | 必需 | 描述 | 示例 |
|------|------|------|------|------|
| `session_id` | string | 是 | 会话ID，用于标识特定的会话连接 | "session456" |

**SSE事件格式**
```
data: {"type": "progress", "content": "正在处理您的请求..."}
data: {"type": "result", "content": "项目创建完成"}
```

**响应头**
- `Cache-Control`: no-cache
- `Connection`: keep-alive

**响应状态码**
- `200`: 成功建立SSE连接，开始接收实时消息
- `404`: 未找到对应的容器
- `500`: 建立SSE连接失败

**Section sources**
- [agent_session_notification.rs](file://crates/rcoder/src/handler/agent_session_notification.rs#L1-L378)

### 任务取消
取消正在执行的AI代理任务。

**端点信息**
- **URL**: `/agent/session/cancel`
- **方法**: `POST`
- **标签**: `agent`

**查询参数**
| 参数 | 类型 | 必需 | 描述 | 示例 |
|------|------|------|------|------|
| `project_id` | string | 是 | 项目ID，用于标识特定的项目 | "test_project" |
| `session_id` | string | 否 | 会话ID，用于标识要取消的会话（可选） | "session456" |

**成功响应示例**
```json
{
  "success": true,
  "data": {
    "success": true,
    "session_id": "session456"
  },
  "error": null
}
```

**响应状态码**
- `200`: 成功转发取消请求到容器
- `400`: 请求参数错误
- `404`: 未找到对应的项目或会话
- `500`: 转发取消请求失败

**Section sources**
- [agent_cancel_handler.rs](file://crates/rcoder/src/handler/agent_cancel_handler.rs#L1-L262)

### 停止Agent
停止指定项目的Agent服务。

**端点信息**
- **URL**: `/agent/stop`
- **方法**: `POST`
- **标签**: `agent`

**查询参数**
| 参数 | 类型 | 必需 | 描述 | 示例 |
|------|------|------|------|------|
| `project_id` | string | 是 | 项目ID | "test_project" |

**成功响应示例**
```json
{
  "success": true,
  "data": {
    "success": true,
    "project_id": "test_project",
    "session_id": null,
    "message": "容器已成功销毁"
  },
  "error": null
}
```

**响应状态码**
- `200`: 成功销毁容器
- `400`: 请求参数错误
- `500`: 销毁容器失败

**Section sources**
- [agent_stop_handler.rs](file://crates/rcoder/src/handler/agent_stop_handler.rs#L1-L241)

## Pingora反向代理API
Pingora是项目内置的高性能反向代理，所有真实的代理请求必须发送到Pingora的监听端口，并使用路径前缀形式`/proxy/{port}/{path}`来指定目标后端端口与路径。

### 代理状态查询
查询代理服务的状态。

**端点信息**
- **URL**: `/proxy/status`
- **方法**: `GET`
- **标签**: `proxy`

**成功响应示例**
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

### 代理统计信息
查看代理的统计信息。

**端点信息**
- **URL**: `/proxy/stats`
- **方法**: `GET`
- **标签**: `proxy`

**成功响应示例**
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

### 代理配置查询
查看代理的配置信息。

**端点信息**
- **URL**: `/proxy/config`
- **方法**: `GET`
- **标签**: `proxy`

**成功响应示例**
```json
{
  "listen_port": 8080,
  "default_backend_port": 3000,
  "default_backend_host": "127.0.0.1",
  "load_balancing_algorithm": "round-robin",
  "health_check": {
    "enabled": true,
    "interval_seconds": 5,
    "timeout_seconds": 3,
    "healthy_threshold": 2,
    "unhealthy_threshold": 3
  }
}
```

**Section sources**
- [proxy_api.rs](file://crates/rcoder/src/handler/proxy_api.rs#L1-L195)
- [proxy_handler_api.rs](file://crates/rcoder/src/handler/proxy_handler_api.rs)

## 请求/响应格式
所有API响应都遵循统一的`HttpResult<T>`格式，确保客户端能够一致地处理响应。

### 响应结构
```json
{
  "code": "0000",
  "message": "成功",
  "data": {},
  "tid": "trace-id",
  "success": true
}
```

| 字段 | 类型 | 描述 |
|------|------|------|
| `code` | string | 响应代码，"0000"表示成功 |
| `message` | string | 响应消息 |
| `data` | object | 响应数据，成功时包含具体数据 |
| `tid` | string | 跟踪ID，用于分布式追踪 |
| `success` | boolean | 是否成功，根据code自动计算 |

### 错误响应结构
```json
{
  "code": "INTERNAL001",
  "message": "Internal server error",
  "data": null,
  "tid": "trace-id",
  "success": false
}
```

**Section sources**
- [http_result.rs](file://crates/shared_types/src/model/http_result.rs#L1-L103)

## 认证方法
RCoder API目前采用基于API密钥的认证方法。用户需要在请求头中包含`Authorization`字段。

### 认证方式
- **类型**: Bearer Token
- **请求头**: `Authorization: Bearer <API_KEY>`
- **环境变量**: `ANTHROPIC_API_KEY`用于Claude代理

### 配置示例
```bash
export ANTHROPIC_API_KEY="your-api-key"
export ANTHROPIC_MODEL="claude-3-sonnet-20240229"
```

## 错误处理策略
RCoder API采用统一的错误处理策略，所有错误都通过`HttpResult<T>`格式返回。

### 错误代码规范
| 代码范围 | 描述 |
|--------|------|
| `0000` | 成功 |
| `1xxx` | 客户端错误 |
| `2xxx` | 服务器错误 |
| `3xxx` | 认证错误 |
| `4xxx` | 权限错误 |
| `5xxx` | 内部错误 |

### 错误处理示例
```json
{
  "code": "INTERNAL001",
  "message": "Internal server error",
  "data": null,
  "success": false
}
```

**Section sources**
- [app_error.rs](file://crates/shared_types/src/model/app_error.rs#L1-L65)

## 安全考虑
RCoder API在设计时充分考虑了安全性，采用了多种安全措施。

### 安全特性
- **项目隔离**: 每个对话在独立的项目工作空间中进行，确保安全性
- **容器化**: AI代理在独立的Docker容器中运行，提供进程隔离
- **输入验证**: 所有输入参数都经过严格验证
- **日志记录**: 所有请求和响应都记录在结构化日志中
- **追踪ID**: 每个请求都有唯一的追踪ID，便于审计和调试

### 安全建议
- 使用HTTPS保护API通信
- 定期轮换API密钥
- 限制API密钥的权限范围
- 监控异常的API调用模式

## 速率限制
RCoder API实施了速率限制策略，以防止滥用和确保服务质量。

### 速率限制规则
- **默认限制**: 100次请求/分钟
- **突发限制**: 200次请求/分钟（短时间）
- **IP限制**: 每个IP地址的请求频率限制
- **项目限制**: 每个项目ID的并发请求限制

### 速率限制响应
当超过速率限制时，API会返回`429 Too Many Requests`状态码。

```json
{
  "code": "RATE_LIMIT_EXCEEDED",
  "message": "请求频率超过限制",
  "data": null,
  "success": false
}
```

## 版本信息
RCoder API遵循语义化版本控制，确保向后兼容性。

### 当前版本
- **API版本**: 1.0.0
- **协议版本**: ACP v0.4
- **Rust版本**: 2024 Edition

### 版本策略
- 主版本号变更表示不兼容的API更改
- 次版本号变更表示向后兼容的功能新增
- 修订号变更表示向后兼容的问题修正

## 客户端实现指南
本节提供客户端实现的建议和最佳实践。

### HTTP客户端配置
- 使用连接池提高性能
- 设置合理的超时时间
- 启用压缩（gzip）减少传输数据量
- 实现重试机制处理临时错误

### SSE客户端实现
```javascript
const eventSource = new EventSource('/agent/progress/session123');
eventSource.onmessage = function(event) {
  console.log('收到消息:', event.data);
};
eventSource.onerror = function(event) {
  console.error('SSE连接错误:', event);
};
```

### 错误处理
- 捕获并处理所有API错误
- 实现指数退避重试策略
- 记录错误日志用于调试
- 向用户提供友好的错误消息

## 性能优化技巧
本节提供性能优化的建议，帮助提高API调用效率。

### 批量请求
- 尽可能使用批量操作减少网络往返
- 合并多个小请求为一个大请求
- 使用长连接保持会话状态

### 缓存策略
- 缓存频繁访问的静态数据
- 使用ETag实现条件请求
- 实现客户端缓存避免重复请求

### 并发处理
- 使用异步调用提高吞吐量
- 实现请求管道化
- 使用Web Worker处理后台任务

## 调试工具与监控方法
本节介绍调试和监控RCoder API的方法。

### 调试工具
- **REST Client**: 使用VS Code的REST Client插件测试API
- **curl**: 命令行工具测试API端点
- **Postman**: 图形化API测试工具

### 监控方法
- **日志监控**: 使用`RUST_LOG=debug`启用详细日志
- **追踪系统**: 集成OpenTelemetry进行分布式追踪
- **指标监控**: 收集API调用指标用于性能分析
- **健康检查**: 定期调用`/health`端点验证服务状态

**Section sources**
- [http_test.rest](file://http_test.rest#L1-L109)
- [README.md](file://README.md#L1-L652)