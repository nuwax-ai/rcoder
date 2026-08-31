//! Agent执行任务的SSE通知处理器
//!
//! 使用 Axum SSE 代理处理 SSE 消息，实现高效的 SSE 转发
//!
//! 模块组织：
//! - utoipa 文档结构体（`ProgressEventDoc` / `SseErrorEvent`）见 [`super::docs`]
//! - SSE 流构建（`SseStreamParams` / `build_sse_stream_from_container_name`）见 [`super::sse_builder`]
//! - 本文件保留 2 个 handler 入口、会话校验与错误映射

mod params;
mod resolve;

pub use params::SessionNotificationParams;
use resolve::validate_and_get_session_context;

use super::sse_builder::{SseStreamParams, build_sse_stream_from_container_name};
use super::utils::I18nPath;
use crate::AppError;
use crate::HttpResult;
use axum::{
    extract::State,
    response::sse::{Event, Sse},
};
use futures_util::stream::Stream;
use std::{convert::Infallible, sync::Arc};
use tracing::{debug, info};

#[utoipa::path(
    get,
    path = "/agent/progress/{session_id}",
    // inline 声明 (与项目其他 handler 一致): 避免引用类型时 utoipa 宏展开
    // 生成限定路径触发 unused_qualifications
    params(
        ("session_id" = String, Path, description = "会话ID，用于标识特定的会话连接"),
        ("pod_id" = Option<String>, Query, description = "Pod ID，用于共享容器模式下的容器定位（可选）"),
        ("tenant_id" = Option<String>, Query, description = "租户ID（可选）"),
        ("space_id" = Option<String>, Query, description = "空间ID（可选）"),
        ("isolation_type" = Option<String>, Query, description = "隔离类型（可选），如 project, tenant, space"),
        ("last_seq" = Option<u64>, Query, description = "客户端消费游标：重连时带上最后收到的 seq 做增量补齐（可选）")
    ),
    responses(
        (
            status = 200,
            description = r#"成功建立 SSE 连接，开始接收实时消息

## 📡 SSE 事件格式

返回标准的 Server-Sent Events (SSE) 流，每个事件包含：

```
event: <sub_type>
data: <payload_json>

```

其中：
- **event**: 事件类型（对应 `ProgressEventDoc.sub_type`）
- **data**: JSON 格式的事件载荷（对应 `ProgressEventDoc.payload`）

## 🔄 事件类型示例

### 1. agent_message_chunk - AI 响应文本片段
```
event: agent_message_chunk
data: {"content":{"type":"text","text":"正在分析您的请求..."},"index":0}
```

### 2. tool_call - 工具调用
```
event: tool_call
data: {"tool_name":"read_file","tool_input":{"path":"src/main.rs"},"status":"started"}
```

### 3. tool_result - 工具执行结果
```
event: tool_result
data: {"tool_name":"read_file","tool_output":"fn main() {...}","status":"success"}
```

### 4. end_turn - 对话轮次结束
```
event: end_turn
data: {"reason":"complete","final_message":"任务已完成"}
```

### 5. error - 错误事件
```
event: error
data: {"code":"EXECUTION_ERROR","message":"执行失败"}
```

## 💡 使用方式

### JavaScript 示例
```javascript
const eventSource = new EventSource('/agent/progress/session123');

// 监听特定事件类型
eventSource.addEventListener('agent_message_chunk', (event) => {
  const data = JSON.parse(event.data);
  console.log('AI 响应:', data.content.text);
});

eventSource.addEventListener('tool_call', (event) => {
  const data = JSON.parse(event.data);
  console.log('工具调用:', data.tool_name, data.tool_input);
});

eventSource.addEventListener('end_turn', (event) => {
  const data = JSON.parse(event.data);
  console.log('任务完成:', data.final_message);
  eventSource.close();
});

// 监听所有消息
eventSource.onmessage = (event) => {
  console.log('收到消息:', event.data);
};

// 错误处理
eventSource.onerror = (error) => {
  console.error('连接错误:', error);
  eventSource.close();
};
```

详细的事件结构请参考 `ProgressEventDoc` schema。"#,
            content_type = "text/event-stream",
            headers(
                ("Cache-Control" = String, description = "no-cache"),
                ("Connection" = String, description = "keep-alive"),
                ("X-Accel-Buffering" = String, description = "no"),
            )
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "未找到对应的容器",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "CONTAINER_NOT_FOUND",
                    "message": "未找到 session_id 对应的活跃容器"
                }
            })
        ),
        (
            status = 500,
            description = "建立 SSE 连接失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "SSE_CONNECTION_ERROR",
                    "message": "无法连接到容器的 SSE 端点"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_session_notification",
    summary = "Agent 会话 SSE 通知流",
    description = r#"建立到指定 session_id 对应容器的 SSE 连接，实时接收 Agent 执行进度和状态更新。

## 🎯 核心概念

此接口返回一个持久化的 SSE (Server-Sent Events) 流，用于实时推送 Agent 的执行进度。客户端应使用 `EventSource` API 或等效的 SSE 客户端库连接此端点。

## 🔄 工作流程

1. 客户端调用 `/chat` 接口发起对话，获得 `session_id`
2. 立即连接 `/agent/progress/{session_id}` 建立 SSE 流
3. 实时接收各类进度事件（文本生成、工具调用等）
4. 收到 `end_turn` 或 `error` 事件后关闭连接

## 📊 事件结构

所有事件都遵循 `ProgressEventDoc` 的结构，包含以下核心字段：
- `message_type`: 主类型（SessionPromptStart, AgentSessionUpdate 等）
- `sub_type`: 子类型，作为 SSE 的 event 字段
- `payload`: JSON 载荷，作为 SSE 的 data 字段
- `timestamp`: 事件时间戳

详细的事件格式和示例请参考响应描述中的 "SSE 事件格式" 部分。"#
)]
pub async fn agent_session_notification(
    I18nPath(params): I18nPath<SessionNotificationParams>,
    State(state): State<Arc<crate::router::AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>> + use<>>, AppError> {
    info!(
        "[SSE_PROXY] Received SSE connection request: session_id={:?}",
        params.session_id
    );
    // 路由定制：按 project 实际 service_type 选流标签与诊断标识——
    // ComputerAgentRunner → user_id，其余 → project_id（Web 路径断流时也能
    // 做 OOM/crashloop 精准诊断）；流标签用实际值（原硬编码 WebAgentRunner，
    // computer 项目的流日志标签错——深审修复）。
    sse_notification_impl(
        state,
        &params,
        &headers,
        Box::new(|state, project_id| match state.get_project(project_id) {
            Some(project) => {
                let service_type = project
                    .service_type()
                    .unwrap_or(shared_types::ServiceType::WebAgentRunner);
                let identifier = match &service_type {
                    shared_types::ServiceType::ComputerAgentRunner => {
                        project.user_id().map(|u| u.to_string())
                    }
                    _ => Some(project_id.to_string()),
                };
                SseRouteContext {
                    service_type: service_type.clone(),
                    diag_ctx: identifier.map(|identifier| {
                        Arc::new(crate::handler::utils::DiagCtx {
                            runtime: state.runtime().clone(),
                            identifier,
                            service_type,
                        })
                    }),
                }
            }
            None => SseRouteContext {
                service_type: shared_types::ServiceType::WebAgentRunner,
                diag_ctx: None,
            },
        }),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/computer/agent/progress/{session_id}",
    // inline 声明 (与项目其他 handler 一致): 避免引用类型时 utoipa 宏展开
    // 生成限定路径触发 unused_qualifications
    params(
        ("session_id" = String, Path, description = "会话ID，用于标识特定的会话连接"),
        ("pod_id" = Option<String>, Query, description = "Pod ID，用于共享容器模式下的容器定位（可选）"),
        ("tenant_id" = Option<String>, Query, description = "租户ID（可选）"),
        ("space_id" = Option<String>, Query, description = "空间ID（可选）"),
        ("isolation_type" = Option<String>, Query, description = "隔离类型（可选），如 project, tenant, space"),
        ("last_seq" = Option<u64>, Query, description = "客户端消费游标：重连时带上最后收到的 seq 做增量补齐（可选）")
    ),
    responses(
        (
            status = 200,
            description = r#"成功建立 SSE 连接，开始接收实时消息

## 📡 SSE 事件格式

与 `/agent/progress/{session_id}` 返回相同的 SSE 流格式。详细说明请参考该接口的文档。

## 🎯 核心特性

- 使用与标准 Agent 相同的事件结构（`ProgressEventDoc`）
- 支持桌面环境中的所有工具调用事件
- 实时推送 AI 响应和工具执行状态

事件类型和使用方式请参考 `agent_session_notification` 接口文档。"#,
            content_type = "text/event-stream",
            headers(
                ("Cache-Control" = String, description = "no-cache"),
                ("Connection" = String, description = "keep-alive"),
                ("X-Accel-Buffering" = String, description = "no"),
            )
        ),
        (
            status = 404,
            description = "未找到对应的容器",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "CONTAINER_NOT_FOUND",
                    "message": "未找到 session_id 对应的活跃容器"
                }
            })
        ),
        (
            status = 500,
            description = "建立 SSE 连接失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "SSE_CONNECTION_ERROR",
                    "message": "无法连接到容器的 SSE 端点"
                }
            })
        )
    ),
    tag = "computer",
    operation_id = "computer_agent_progress_notification",
    summary = "Computer Agent 会话进度 SSE 流",
    description = r#"为 Computer Agent 专用的进度流接口，建立 SSE 连接实时接收执行进度和状态更新。

此接口与 `/computer/progress/{session_id}` 功能相同，提供更明确的路径结构。

## 🔄 核心逻辑

该接口与 `agent_session_notification` 使用相同的数据验证和查找逻辑：

1. 验证会话ID对应的容器是否存在
2. 检查容器是否正在运行
3. 查找对应的项目和代理信息
4. 建立 gRPC SSE 连接

所有验证逻辑都通过 `validate_and_get_session_context` 函数统一处理。

## 📊 事件结构

返回的 SSE 事件遵循 `ProgressEventDoc` 结构，与标准 Agent 接口完全一致。详细的事件类型和使用示例请参考 `/agent/progress/{session_id}` 接口文档。"#
)]
pub async fn computer_agent_progress_notification(
    I18nPath(params): I18nPath<SessionNotificationParams>,
    State(state): State<Arc<crate::router::AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>> + use<>>, AppError> {
    info!(
        "[SSE_PROXY] Received Computer Agent SSE connection request: session_id={:?}",
        params.session_id
    );
    // 路由定制：computer 入口默认 ComputerAgentRunner 语义；但 UserappBuilder
    // 项目（userapp dev chat 的 SSE 经 /devcomputer 委托本入口）按 project 实际
    // service_type 分发——容器定位 identifier 取 project_id（app_id），否则断流
    // 诊断会查 user 的 computer 容器而非 app 开发容器（与通用入口对齐）
    sse_notification_impl(
        state,
        &params,
        &headers,
        Box::new(|state, project_id| match state.get_project(project_id) {
            Some(project) => {
                let service_type = project
                    .service_type()
                    .unwrap_or(shared_types::ServiceType::ComputerAgentRunner);
                let identifier = match &service_type {
                    shared_types::ServiceType::ComputerAgentRunner => {
                        project.user_id().map(|u| u.to_string())
                    }
                    _ => Some(project_id.to_string()),
                };
                SseRouteContext {
                    service_type: service_type.clone(),
                    diag_ctx: identifier.map(|identifier| {
                        Arc::new(crate::handler::utils::DiagCtx {
                            runtime: state.runtime().clone(),
                            identifier,
                            service_type,
                        })
                    }),
                }
            }
            None => SseRouteContext {
                service_type: shared_types::ServiceType::ComputerAgentRunner,
                diag_ctx: None,
            },
        }),
    )
    .await
}

/// 两个 SSE 通知 handler 的共享实现（验证 → activity_updater → last_seq →
/// SseStreamParams → build 流）。两入口仅在**路由定制**上有差异——流标签
/// `service_type` 与断流诊断 `diag_ctx` 的选取策略，经 [`SseRouteContext`]
/// 闭包注入（validate 之后才有 project_id，故闭包以 project_id 为入参）。
struct SseRouteContext {
    service_type: shared_types::ServiceType,
    diag_ctx: Option<Arc<crate::handler::utils::DiagCtx>>,
}

/// 路由定制闭包类型（validate 后以 project_id 调用，产出流标签与诊断标识）。
type RouteCtxFn = Box<dyn FnOnce(&Arc<crate::router::AppState>, &str) -> SseRouteContext + Send>;

async fn sse_notification_impl(
    state: Arc<crate::router::AppState>,
    params: &SessionNotificationParams,
    headers: &axum::http::HeaderMap,
    route_ctx: RouteCtxFn,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>> + use<>>, AppError> {
    let locale = shared_types::current_request_locale();
    let session_id = &params.session_id;

    let (project_id, container_name, container_ip) =
        validate_and_get_session_context(state.clone(), session_id).await?;

    // 活跃时间更新闭包（捕获 state 引用）；SSE 流收到非心跳事件时节流调用。
    let activity_updater: Arc<dyn Fn(&str) + Send + Sync> = {
        let state = state.clone();
        Arc::new(move |sid: &str| state.update_session_activity(sid))
    };

    // 游标：优先 Last-Event-ID header（浏览器 EventSource 断线重连自动带），
    // 其次 ?last_seq= query，缺省从头。
    let last_seq = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or(params.last_seq)
        .unwrap_or(0);

    let route = route_ctx(&state, &project_id);
    let stream_params = SseStreamParams {
        container_name,
        container_ip,
        session_id: session_id.to_string(),
        project_id,
        grpc_pool: state.grpc_pool.clone(),
        locale,
        service_type: route.service_type,
        activity_updater,
        namespace: state.config.app_manager.namespace.clone(),
        cluster_domain: state.cluster_domain.clone(),
        registry: state.session_stream_registry.clone(),
        diag_ctx: route.diag_ctx,
        last_seq,
    };
    build_sse_stream_from_container_name(stream_params).await
}

/// 创建错误（渲染交 [`AppError::into_response`]：status 由错误码推导、body 为
/// HttpResult 信封 + locale 感知，具体 message 进响应与 `error!` 日志）。
///
/// 行为说明（相对旧 `create_error_response` 手拼形态）：HTTP status 与响应 shape
/// 等价（同一错误码体系）；body 的 message 从"错误码通用 i18n 文案"变为具体的
/// `message` 参数（信息量更大）；`INVALID_DATA` 旧调用点曾传 500 但映射后语义
/// 是参数错误，现统一为 400（修正存量 status/code 矛盾）。
pub(super) fn create_session_error(code: &str, message: &str) -> AppError {
    let mapped_code = map_error_code_for_locale(code);
    debug!(
        "[SSE_PROXY] create session error: code={} -> mapped={}, message={}",
        code, mapped_code, message
    );
    AppError::with_message(mapped_code, message)
}

pub(super) fn map_error_code_for_locale(code: &str) -> &str {
    use shared_types::error_codes;

    match code {
        "SESSION_NOT_FOUND" | "SESSION_EXPIRED" => error_codes::ERR_SESSION_NOT_FOUND,
        "CONTAINER_NOT_FOUND" => error_codes::ERR_CONTAINER_NOT_FOUND,
        "GRPC_CONNECTION_ERROR" => error_codes::ERR_GRPC_ERROR,
        "CONTAINER_ERROR" => error_codes::ERR_CONTAINER_ERROR,
        "INVALID_DATA" => error_codes::ERR_INVALID_PARAMS,
        error_codes::ERR_INTERNAL_SERVER_ERROR => error_codes::ERR_INTERNAL_SERVER_ERROR,
        _ => error_codes::ERR_UNKNOWN,
    }
}
