//! Agent执行任务的SSE通知处理器
//!
//! 使用 Axum SSE 代理处理 SSE 消息，实现高效的 SSE 转发
//!
//! 模块组织：
//! - utoipa 文档结构体（`ProgressEventDoc` / `SseErrorEvent`）见 [`super::docs`]
//! - SSE 流构建（`SseStreamParams` / `build_sse_stream_from_container_name`）见 [`super::sse_builder`]
//! - 本文件保留 2 个 handler 入口、会话校验与错误映射

use super::sse_builder::{SseStreamParams, build_sse_stream_from_container_name};
use super::utils::{I18nPath, container_identity_from_name};
use crate::HttpResult;
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        Response,
        sse::{Event, Sse},
    },
};
use futures_util::stream::Stream;
use serde::Deserialize;
use shared_types::ProjectAndContainerInfo;
use std::{convert::Infallible, sync::Arc};
use tracing::{debug, error, info, warn};
use utoipa::{IntoParams, ToSchema};

/// 会话通知路径参数
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
// pod_id/tenant_id/space_id/isolation_type 是 API 契约预留参数（前端可传，
// 当前服务端未消费），不可删除，故显式压制 dead_code 警告
#[allow(dead_code)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    #[param(example = "session456")]
    pub session_id: String,
    /// Pod ID，用于共享容器模式下的容器定位（可选）
    #[param(example = "pod_abc123")]
    #[serde(default)]
    pub pod_id: Option<String>,
    /// 租户ID（可选）
    #[param(example = "tenant_001")]
    #[serde(
        default,
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    pub tenant_id: Option<String>,
    /// 空间ID（可选）
    #[param(example = "space_001")]
    #[serde(
        default,
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    pub space_id: Option<String>,
    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[param(example = "project")]
    #[serde(default)]
    pub isolation_type: Option<String>,
    /// 客户端消费游标（可选）：重连时带上最后收到的 seq（`ProgressEvent.seq`），
    /// rcoder 只补齐 seq > last_seq 的消息（增量补齐，消除重复）。
    /// 缺省 = 补齐该 session 全量历史（首次连接合理；重连建议前端带上）。
    #[param(example = "12")]
    #[serde(default)]
    pub last_seq: Option<u64>,
}

/// 核心验证函数：验证会话并获取容器名称
///
/// 这个函数被 SSE 通知处理器使用
/// 执行所有必要的验证和查找逻辑，但不执行实际的消息流创建
///
/// 🔧 关键修复：使用稳定的 container_name 替代 container_id 查询容器状态
/// 当容器被重启后，container_id 会变化，但 container_name 保持稳定。
///
/// 返回: (project_id, container_name)
async fn validate_and_get_session_context(
    state: Arc<crate::router::AppState>,
    session_id: &str,
) -> Result<(String, String, String), Response> {
    // ========== 阶段 1: 获取项目信息（所有分支都需要） ==========
    // 🔧 优化：提前获取 project_info，避免后续重复查询
    // 同时获取 DockerManager（用于容器验证和降级查询）
    let project_info = lookup_project_info_by_session(&state, session_id)?;

    let runtime = state.runtime().clone();

    // ========== 阶段 2: 获取稳定的 container_name（不是 container_id） ==========
    // 🔧 关键修复：container_name 在容器重建后保持不变（如 computer-agent-runner-user_123）
    // 而 container_id 在每次容器重建后都会变化
    let mut container_name = match state.get_container_name_by_session(session_id) {
        Some(name) => {
            debug!(
                " [SSE_PROXY] Getting container name from storage: session_id={}, container_name={}",
                session_id, name
            );
            name
        }
        None => resolve_container_name_fallback(&project_info, &runtime, session_id).await?,
    };

    // ========== 阶段 3: 优先使用内存中的容器信息，避免不必要的 Docker API 调用 ==========
    container_name =
        verify_container_with_memory_preference(&state, &runtime, &project_info, container_name)
            .await?;

    // ========== 阶段 4: 返回验证通过的上下文 ==========

    // 🎯 优化：直接使用阶段 1 中已获取的 project_info，避免重复查询
    let project_id = project_info.project_id().to_string();

    // 获取 container_ip（Docker 环境需要）
    let container_ip = project_info
        .container_info()
        .map(|c| c.container_ip.clone())
        .unwrap_or_default();

    // 注意：由于阶段 3 已经处理了 project_info.container_info() 为 None 的情况
    // （通过 Docker API 降级查询），这里无需再次验证容器信息的完整性
    info!(
        " [SSE_PROXY] All validations passed: session_id={}, project_id={}, container_name={}, container_ip={}",
        session_id, project_id, container_name, container_ip
    );
    Ok((project_id, container_name, container_ip))
}

/// 阶段 1：按 session_id 查找项目信息
#[allow(clippy::result_large_err)]
fn lookup_project_info_by_session(
    state: &Arc<crate::router::AppState>,
    session_id: &str,
) -> Result<Arc<ProjectAndContainerInfo>, Response> {
    match state.get_by_session(session_id) {
        Some(info) => {
            debug!(
                " [SSE_PROXY] Getting project info from memory: session_id={}, project_id={}",
                session_id,
                info.project_id()
            );
            Ok(info)
        }
        None => {
            error!(
                " [SSE_PROXY] Project info for session not found: session_id={}",
                session_id
            );
            Err(create_error_response(
                StatusCode::NOT_FOUND,
                "SESSION_NOT_FOUND",
                "Session does not exist or has expired. Please submit a new request.",
            ))
        }
    }
}

/// 阶段 2 降级：存储中没有 container_name 记录时的实时查询
///
/// 可能原因：
/// 1. 新 session 尚未写入 存储（正常情况）
/// 2. 测试环境脏数据
/// 3. 容器重建后 存储 未更新
async fn resolve_container_name_fallback(
    project_info: &Arc<ProjectAndContainerInfo>,
    runtime: &Arc<dyn container_runtime_api::ContainerRuntime>,
    session_id: &str,
) -> Result<String, Response> {
    info!(
        " [SSE_PROXY] session_id record not found in storage, executing fallback query: session_id={}, project_id={}",
        session_id,
        project_info.project_id()
    );

    // 根据 service_type 选择不同的查询策略
    match project_info.service_type() {
        Some(shared_types::ServiceType::ComputerAgentRunner) => {
            // ComputerAgentRunner 模式：通过 user_id 查询容器
            resolve_container_name_by_user_id(project_info, runtime, session_id).await
        }
        _ => {
            // RCoder 模式：从 project_info 获取容器名称，或使用 project_id 作为容器名称
            //
            // ⚠️ 注意：project_info 从 存储 读取，可能包含部分过时数据
            // - container_name: 稳定不变（容器重建后仍有效）
            // - container_id, container_ip: 可能过时（容器重建后会变化）
            //
            // 阶段 3 会验证容器的真实存在性（通过内存信息或 Docker API）
            // 因此即使 project_info.container_info() 为 None，也可以继续执行
            match project_info.container_info() {
                Some(container) => {
                    info!(
                        " [SSE_PROXY] Fallback query succeeded: got container name from project_info: container_name={}",
                        container.container_name
                    );
                    Ok(container.container_name.clone())
                }
                None => {
                    // project_info 中没有容器信息，使用 project_id 作为容器名称
                    // 这通常发生在容器刚创建但尚未写入 存储 的情况
                    // 阶段 3 会通过 Docker API 验证容器是否存在
                    warn!(
                        " [SSE_PROXY] No container info in project_info, using project_id as container name: project_id={}",
                        project_info.project_id()
                    );
                    Ok(project_info.project_id().to_string())
                }
            }
        }
    }
}

/// ComputerAgentRunner 模式降级：通过 user_id 实时查询容器
async fn resolve_container_name_by_user_id(
    project_info: &Arc<ProjectAndContainerInfo>,
    runtime: &Arc<dyn container_runtime_api::ContainerRuntime>,
    session_id: &str,
) -> Result<String, Response> {
    let Some(user_id) = project_info.user_id() else {
        error!(
            "[SSE_PROXY] Missing user_id in ComputerAgentRunner mode: session_id={}",
            session_id
        );
        return Err(create_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INVALID_DATA",
            "Project missing user identifier",
        ));
    };

    match runtime
        .get_container_info_by_identifier(user_id, &shared_types::ServiceType::ComputerAgentRunner)
        .await
    {
        Ok(Some(info)) => {
            info!(
                " [SSE_PROXY] Fallback query succeeded: getting container via user_id in real-time: user_id={}, container_name={}",
                user_id, info.container_name
            );
            Ok(info.container_name)
        }
        Ok(None) => {
            error!(
                "[SSE_PROXY] Fallback query failed: container not found: user_id={}",
                user_id
            );
            Err(create_error_response(
                StatusCode::NOT_FOUND,
                "CONTAINER_NOT_FOUND",
                &format!("container not found: user_id={}", user_id),
            ))
        }
        Err(e) => {
            error!(
                "[SSE_PROXY] Fallback query failed: failed to query container: {}",
                e
            );
            Err(create_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONTAINER_ERROR",
                &format!("Failed to query container: {}", e),
            ))
        }
    }
}

/// 阶段 3：优先使用内存中的容器信息，避免不必要的 Docker API 调用
///
/// 🎯 优化策略：
/// 1. 首先检查内存中的 project_info.container_info() 是否已存在
/// 2. 如果存在 → 使用内存中的 container_name（它是最新的），跳过 Docker API 调用
/// 3. 如果不存在 → 调用 runtime 实时查询 作为降级方案
/// 4. 后续会通过 gRPC GetStatus 进行最终健康检查
async fn verify_container_with_memory_preference(
    state: &Arc<crate::router::AppState>,
    runtime: &Arc<dyn container_runtime_api::ContainerRuntime>,
    project_info: &Arc<ProjectAndContainerInfo>,
    container_name: String,
) -> Result<String, Response> {
    if let Some(container) = project_info.container_info() {
        info!(
            " [SSE_PROXY] Using container info from memory: container_name={}, container_ip={}",
            container.container_name, container.container_ip
        );
        // 🎯 关键修复：使用内存中的 container_name（它是最新的）
        // storage 中的 container_name 可能对应旧容器（如 user container）
        // 内存中的 container_name 对应当前活跃的容器（如 project container）
        return Ok(container.container_name.clone());
    }

    // 内存中没有容器信息，调用 Docker API 实时查询
    warn!(
        " [SSE_PROXY] Container info missing in memory, calling runtime query: container_name={}",
        container_name
    );
    let computer_prefix = &state.container_prefix_computer;
    let rcoder_prefix = &state.container_prefix_rcoder;
    let query = if let Some((id, service_type)) =
        container_identity_from_name(&container_name, rcoder_prefix, computer_prefix)
    {
        runtime.find_container(id, &service_type).await
    } else {
        runtime
            .find_container(
                project_info.project_id(),
                &shared_types::ServiceType::WebAgentRunner,
            )
            .await
    };
    match query {
        Ok(Some(result)) => {
            if result.status == container_runtime_api::ContainerRuntimeStatus::Running {
                info!(
                    " [SSE_PROXY] Runtime query successful, container is running: container_name={}",
                    container_name
                );
                Ok(container_name)
            } else {
                Err(create_error_response(
                    StatusCode::NOT_FOUND,
                    "SESSION_EXPIRED",
                    "Session has been cleaned up due to inactivity. Please submit a new request.",
                ))
            }
        }
        Ok(None) => {
            error!(
                " [SSE_PROXY] Container does not exist: container_name={}",
                container_name
            );
            Err(create_error_response(
                StatusCode::NOT_FOUND,
                "SESSION_EXPIRED",
                "Container not found. Please submit a new request.",
            ))
        }
        Err(e) => {
            error!(" [SSE_PROXY] Runtime query failed: {}", e);
            Err(create_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                "Error checking session status. Please retry later.",
            ))
        }
    }
}

/// Agent 会话 SSE 通知处理器
///
/// 此接口直接返回 SSE 流，实现从容器到客户端的实时消息转发
///
/// ## 🔄 代理流程
///
/// 1. 用户请求 `/agent/progress/{session_id}`
/// 2. axum 处理器检查 session_id 对应的容器是否存在
/// 3. 建立到容器 SSE 端点的连接
/// 4. 将容器的 SSE 流直接转发给客户端
/// 5. 保持连接直到客户端断开或容器停止
///
/// ## 💡 优势
///
/// - **实时性**: 直接转发 SSE 流，保持原始协议特性
/// - **透明代理**: 客户端无感知的容器连接
/// - **错误处理**: 完善的连接错误和重试机制
/// - **资源管理**: 自动清理断开的连接
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
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let locale = shared_types::current_request_locale();
    let session_id = &params.session_id;
    info!(
        " [SSE_PROXY] Received SSE connection request: session_id={:?}",
        session_id
    );

    // 使用核心验证函数获取上下文
    let (project_id, container_name, container_ip) =
        validate_and_get_session_context(state.clone(), session_id).await?;

    // 构造活跃时间更新闭包（捕获 state 引用）
    // Bug 5 修复：SSE 流收到非心跳事件时节流调用此闭包
    let activity_updater: Arc<dyn Fn(&str) + Send + Sync> = {
        let state = state.clone();
        Arc::new(move |sid: &str| state.update_session_activity(sid))
    };

    // 使用通用函数创建 SSE 响应流
    // 优先用 Last-Event-ID header（浏览器 EventSource 断线重连自动带），其次 ?last_seq= query
    let last_seq = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or(params.last_seq)
        .unwrap_or(0);
    let params = SseStreamParams {
        container_name,
        container_ip,
        session_id: session_id.to_string(),
        project_id,
        grpc_pool: state.grpc_pool.clone(),
        locale,
        service_type: shared_types::ServiceType::WebAgentRunner,
        activity_updater,
        namespace: state.config.app_manager.namespace.clone(),
        cluster_domain: state.cluster_domain.clone(),
        registry: state.session_stream_registry.clone(),
        last_seq,
    };
    build_sse_stream_from_container_name(params).await
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
    summary = "Computer Agent 专用会话 SSE 通知流",
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
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let locale = shared_types::current_request_locale();
    let session_id = &params.session_id;
    info!(
        " [SSE_PROXY] Received Computer Agent SSE connection request: session_id={:?}",
        session_id
    );

    // 使用与 agent_session_notification 相同的验证逻辑
    let (project_id, container_name, container_ip) =
        validate_and_get_session_context(state.clone(), session_id).await?;

    // 构造活跃时间更新闭包（捕获 state 引用）
    // Bug 5 修复：SSE 流收到非心跳事件时节流调用此闭包
    let activity_updater: Arc<dyn Fn(&str) + Send + Sync> = {
        let state = state.clone();
        Arc::new(move |sid: &str| state.update_session_activity(sid))
    };

    // 使用通用函数创建 SSE 响应流
    // 优先用 Last-Event-ID header（浏览器 EventSource 断线重连自动带），其次 ?last_seq= query
    let last_seq = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or(params.last_seq)
        .unwrap_or(0);
    let params = SseStreamParams {
        container_name,
        container_ip,
        session_id: session_id.to_string(),
        project_id,
        grpc_pool: state.grpc_pool.clone(),
        locale,
        service_type: shared_types::ServiceType::ComputerAgentRunner,
        activity_updater,
        namespace: state.config.app_manager.namespace.clone(),
        cluster_domain: state.cluster_domain.clone(),
        registry: state.session_stream_registry.clone(),
        last_seq,
    };
    build_sse_stream_from_container_name(params).await
}

/// 创建错误响应
fn create_error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let locale = shared_types::current_request_locale();
    let mapped_code = map_error_code_for_locale(code);
    let localized_message = shared_types::get_error_message(mapped_code, locale);
    let error_body = HttpResult::<()>::error(mapped_code, &localized_message);
    let json_body = serde_json::to_string(&error_body).unwrap_or_default();

    debug!(
        "[SSE_PROXY] create error response: code={}, status={}, locale={}, original_message={}",
        code, status, locale, message
    );

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(json_body.into())
        .unwrap_or_else(|_| Response::new("Internal Server Error".into()))
}

fn map_error_code_for_locale(code: &str) -> &str {
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
