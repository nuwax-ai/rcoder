//! Agent执行任务的SSE通知处理器
//!
//! 使用 Axum SSE 代理处理 SSE 消息，实现高效的 SSE 转发

use super::utils::{I18nPath, get_realtime_container_ip_with_cache};
use crate::{AppError, HttpResult};
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::{StreamExt, stream::Stream};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use shared_types::ProjectAndContainerInfo;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};
use utoipa::{IntoParams, ToSchema};

/// 会话通知路径参数
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    #[param(example = "session456")]
    pub session_id: String,
}

/// SSE 进度事件（用于 OpenAPI 文档）
///
/// 这是通过 SSE 流推送的实际事件结构，遵循标准 SSE 格式。
/// SSE 的 `data` 字段使用 `UnifiedSessionMessage` 结构体，包含完整的会话上下文信息：
///
/// ```text
/// event: agent_message_chunk
/// data: {"session_id":"session456","message_type":"AgentSessionUpdate","sub_type":"agent_message_chunk","data":{"content":{"type":"text","text":"Hello"},"index":0},"timestamp":"2024-12-16T10:30:00Z"}
///
/// event: tool_call
/// data: {"session_id":"session456","message_type":"AgentSessionUpdate","sub_type":"tool_call","data":{"tool_name":"read_file","tool_input":{"path":"test.rs"},"status":"started"},"timestamp":"2024-12-16T10:30:01Z"}
///
/// event: end_turn
/// data: {"session_id":"session456","message_type":"AgentSessionUpdate","sub_type":"end_turn","data":{"reason":"EndTurn","description":"正常结束"},"timestamp":"2024-12-16T10:30:05Z"}
/// ```
///
/// ---
///
/// ## 📝 重要说明
///
/// | 项目 | 说明 |
/// |------|------|
/// | **结构体用途** | 用于 OpenAPI 文档展示，描述 gRPC `ProgressEvent` 的完整信息 |
/// | **实际 SSE 格式** | 只有 `event` (= `sub_type`) 和 `data` (= `payload`) 两个字段 |
/// | **payload 类型** | 文档中为 `Value`（便于展示），实际传输为 JSON 字符串 |
/// | **元数据传输** | `message_type`, `request_id`, `timestamp` 在 gRPC 层传输，不直接出现在 SSE 流中 |
/// | **前端接收** | 使用 `EventSource`，通过 `event.type` 和 `event.data` 获取数据 |
///
/// ---
///
/// ## 🔄 数据流转换链路
///
/// ```text
/// [agent_runner]                    [rcoder]                      [前端]
/// UnifiedSessionMessage  ──gRPC──>  ProgressEvent  ──SSE──>  EventSource
///      │                                 │                        │
///      ├─ session_id ────────────────────┼────────> URL 路径传递   │
///      ├─ message_type ──────────────────┼────────> (gRPC 元数据)  │
///      ├─ sub_type ──────────────────────┼────────> event 字段 ───┤
///      ├─ data ──────────> payload ──────┼────────> data 字段 ────┤
///      ├─ timestamp ─────────────────────┼────────> (gRPC 元数据)  │
///      └─ request_id (在 data 中) ───────┴────────> (在 payload 中)│
/// ```
///
/// ---
///
/// ## 📊 message_type 与 sub_type 对应关系
///
/// | message_type | sub_type | 说明 |
/// |--------------|----------|------|
/// | `SessionPromptStart` | `prompt_start` | 用户发起对话，Agent 开始处理 |
/// | `SessionPromptEnd` | `end_turn` | Agent 正常完成任务 |
/// | `SessionPromptEnd` | `max_tokens` | 达到最大 token 数限制 |
/// | `SessionPromptEnd` | `max_turn_requests` | 达到最大请求数限制 |
/// | `SessionPromptEnd` | `refusal` | Agent 拒绝继续执行 |
/// | `SessionPromptEnd` | `cancelled` | 用户取消任务 |
/// | `SessionPromptEnd` | `error` | 执行过程中发生错误 |
/// | `AgentSessionUpdate` | `agent_message_chunk` | AI 响应文本片段 |
/// | `AgentSessionUpdate` | `agent_thought_chunk` | AI 思考过程片段 |
/// | `AgentSessionUpdate` | `user_message_chunk` | 用户消息片段 |
/// | `AgentSessionUpdate` | `tool_call` | 工具调用开始 |
/// | `AgentSessionUpdate` | `tool_call_update` | 工具调用状态更新 |
/// | `AgentSessionUpdate` | `plan` | 执行计划更新 |
/// | `AgentSessionUpdate` | `available_commands_update` | 可用命令列表更新 |
/// | `AgentSessionUpdate` | `current_mode_update` | 当前模式更新 |
/// | `Heartbeat` | `ping` | 心跳保活消息 |
///
/// ## 📦 完整 SSE 消息示例
///
/// ```json
/// {
///   "sessionId": "019b262c-e6d2-75d8-a374-2aa08bd93afd",
///   "messageType": "agentSessionUpdate",
///   "subType": "agent_message_chunk",
///   "data": {
///     "content": {"text": "你好，我来帮你...", "type": "text"},
///     "request_id": "d633d7b0ba9d4505ae6d87a5b274c580"
///   },
///   "timestamp": "2025-12-16T08:00:39.766Z"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEventDoc {
    /// 会话ID
    ///
    /// 与 URL 路径中的 `session_id` 参数一致，用于标识当前会话。
    #[schema(example = "019b262c-e6d2-75d8-a374-2aa08bd93afd")]
    pub session_id: String,

    /// 消息主类型
    ///
    /// 用于区分消息的生命周期阶段，便于前端进行状态管理。
    ///
    /// ## 可能的值
    ///
    /// | 值 | 说明 | 对应 subType |
    /// |----|------|--------------|
    /// | `sessionPromptStart` | 会话开始，Agent 开始处理用户请求 | `prompt_start` |
    /// | `sessionPromptEnd` | 会话结束，Agent 完成或终止处理 | `end_turn`, `max_tokens`, `cancelled`, `error` 等 |
    /// | `agentSessionUpdate` | 执行过程中的实时更新 | `agent_message_chunk`, `tool_call`, `plan` 等 |
    /// | `heartbeat` | 心跳消息，用于保持 SSE 连接 | `ping` |
    ///
    /// ## 前端状态机示例
    ///
    /// ```javascript
    /// eventSource.addEventListener('agent_message_chunk', (event) => {
    ///   const msg = JSON.parse(event.data);
    ///   switch (msg.messageType) {
    ///     case 'sessionPromptStart':
    ///       setStatus('processing');
    ///       break;
    ///     case 'agentSessionUpdate':
    ///       handleUpdate(msg.subType, msg.data);
    ///       break;
    ///     case 'sessionPromptEnd':
    ///       setStatus('completed');
    ///       break;
    ///     case 'heartbeat':
    ///       // 忽略或更新最后活跃时间
    ///       break;
    ///   }
    /// });
    /// ```
    #[schema(example = "agentSessionUpdate")]
    pub message_type: String,

    /// 消息子类型（作为 SSE 的 event 字段）
    ///
    /// 这是 SSE 事件的核心标识，前端应根据此字段决定如何处理 `data`。
    ///
    /// ## 完整的 subType 列表
    ///
    /// ### 会话生命周期事件
    /// | subType | messageType | 说明 |
    /// |---------|-------------|------|
    /// | `prompt_start` | sessionPromptStart | 会话开始 |
    /// | `end_turn` | sessionPromptEnd | 正常结束 |
    /// | `max_tokens` | sessionPromptEnd | token 限制 |
    /// | `max_turn_requests` | sessionPromptEnd | 请求数限制 |
    /// | `refusal` | sessionPromptEnd | Agent 拒绝 |
    /// | `cancelled` | sessionPromptEnd | 用户取消 |
    /// | `error` | sessionPromptEnd | 执行错误 |
    ///
    /// ### Agent 执行过程事件
    /// | subType | 说明 | 典型用途 |
    /// |---------|------|----------|
    /// | `agent_message_chunk` | AI 响应文本片段 | 流式显示 AI 回复 |
    /// | `agent_thought_chunk` | AI 思考过程片段 | 显示推理过程（可折叠） |
    /// | `user_message_chunk` | 用户消息片段 | 回显用户输入 |
    /// | `tool_call` | 工具调用开始 | 显示正在执行的操作 |
    /// | `tool_call_update` | 工具调用状态更新 | 显示工具执行结果 |
    /// | `plan` | 执行计划 | 显示任务分解步骤 |
    /// | `available_commands_update` | 可用命令更新 | 更新交互按钮 |
    /// | `current_mode_update` | 模式更新 | 显示当前工作模式 |
    ///
    /// ### 系统事件
    /// | subType | 说明 |
    /// |---------|------|
    /// | `ping` | 心跳保活 |
    ///
    /// ## 前端监听示例
    ///
    /// ```javascript
    /// const eventSource = new EventSource('/agent/progress/session_123');
    ///
    /// // 监听特定事件
    /// eventSource.addEventListener('agent_message_chunk', handleChunk);
    /// eventSource.addEventListener('tool_call', handleToolCall);
    /// eventSource.addEventListener('end_turn', handleComplete);
    /// ```
    #[schema(example = "agent_message_chunk")]
    pub sub_type: String,

    /// ACP 消息的完整 JSON 载荷
    ///
    /// 这是一个 JSON 对象，包含完整的 ACP (Agent Client Protocol) 消息数据。
    /// 具体结构取决于 `subType`，前端应根据 `subType` 解析此 JSON。
    ///
    /// ---
    ///
    /// ## 📋 各 subType 对应的 data 结构
    ///
    /// ### 1. `prompt_start` - 会话开始
    /// ```json
    /// {
    ///   "request_id": "req_123"  // 可选
    /// }
    /// ```
    ///
    /// ### 2. `end_turn` / `max_tokens` / `cancelled` 等 - 会话结束
    /// ```json
    /// {
    ///   "reason": "EndTurn",           // 停止原因枚举值
    ///   "description": "正常结束",      // 人类可读的描述
    ///   "error_message": "...",        // 可选，错误时才有
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    /// **reason 可能的值**: `EndTurn`, `MaxTokens`, `MaxTurnRequests`, `Refusal`, `Cancelled`
    ///
    /// ### 3. `error` - 执行错误
    /// ```json
    /// {
    ///   "code": -1,                    // 错误代码
    ///   "message": "执行失败: ...",     // 错误消息
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 4. `ping` - 心跳消息
    /// ```json
    /// {
    ///   "type": "heartbeat",
    ///   "message": "keep-alive",
    ///   "timestamp": "2024-01-01T00:00:00Z"
    /// }
    /// ```
    ///
    /// ### 5. `agent_message_chunk` - AI 响应文本片段
    /// ```json
    /// {
    ///   "content": {
    ///     "type": "text",              // 内容类型
    ///     "text": "你好，我来帮你..."   // 文本内容
    ///   },
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 6. `agent_thought_chunk` - AI 思考过程片段
    /// ```json
    /// {
    ///   "content": {
    ///     "type": "thinking",
    ///     "thinking": "正在分析用户的请求..."
    ///   },
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 7. `tool_call` - 工具调用
    /// ```json
    /// {
    ///   "tool_use_id": "tool_123",     // 工具调用 ID
    ///   "tool_name": "read_file",      // 工具名称
    ///   "tool_input": {                // 工具输入参数
    ///     "path": "src/main.rs"
    ///   },
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 8. `tool_call_update` - 工具调用状态更新
    /// ```json
    /// {
    ///   "tool_use_id": "tool_123",     // 工具调用 ID
    ///   "status": "running",           // 状态: running, success, error
    ///   "output": "...",               // 可选，工具输出
    ///   "error": "...",                // 可选，错误信息
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 9. `plan` - 执行计划
    /// ```json
    /// {
    ///   "steps": [                     // 计划步骤列表
    ///     {"description": "分析代码结构", "status": "completed"},
    ///     {"description": "修改文件", "status": "in_progress"},
    ///     {"description": "运行测试", "status": "pending"}
    ///   ],
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 10. `available_commands_update` - 可用命令更新
    /// ```json
    /// {
    ///   "available_commands": ["yes", "no", "explain"],
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 11. `current_mode_update` - 当前模式更新
    /// ```json
    /// {
    ///   "current_mode_id": "code_review",
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    #[schema(
        example = json!({
            "content": {
                "type": "text",
                "text": "正在分析您的请求..."
            },
            "request_id": "req_123"
        })
    )]
    pub data: serde_json::Value,

    /// 事件时间戳（ISO 8601 格式）
    ///
    /// ## 格式
    ///
    /// - **类型**: ISO 8601 字符串
    /// - **时区**: UTC（以 `Z` 结尾）
    /// - **精度**: 毫秒
    ///
    /// ## 用途
    ///
    /// - **事件排序**: 确保事件按正确的时间顺序处理
    /// - **延迟计算**: 前端可计算网络延迟
    /// - **超时检测**: 检测是否有事件丢失或延迟过大
    /// - **日志记录**: 记录精确的事件发生时间
    ///
    /// ## 前端使用示例
    ///
    /// ```javascript
    /// eventSource.addEventListener('agent_message_chunk', (event) => {
    ///   const msg = JSON.parse(event.data);
    ///
    ///   // 直接解析 ISO 8601 字符串
    ///   const eventTime = new Date(msg.timestamp);
    ///
    ///   // 计算网络延迟
    ///   const latency = Date.now() - eventTime.getTime();
    ///   console.log(`事件延迟: ${latency}ms`);
    ///
    ///   // 格式化显示
    ///   const timeStr = eventTime.toLocaleTimeString();
    /// });
    /// ```
    ///
    /// ## 注意事项
    ///
    /// - 时间戳在 `agent_runner` 端生成，反映事件的实际发生时间
    /// - 由于网络传输，前端收到事件时可能有几十到几百毫秒的延迟
    #[schema(example = "2025-12-16T08:00:39.766Z")]
    pub timestamp: String,
}

/// SSE 错误事件（用于 OpenAPI 文档）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SseErrorEvent {
    /// 错误代码
    #[schema(example = "GRPC_CONNECTION_ERROR")]
    pub code: String,
    /// 错误消息
    #[schema(example = "无法连接到 Agent 服务")]
    pub message: String,
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
) -> Result<(String, String), Response> {
    // ========== 阶段 1: 获取项目信息（所有分支都需要） ==========
    // 🔧 优化：提前获取 project_info，避免后续重复查询
    // 同时获取 DockerManager（用于容器验证和降级查询）
    let project_info = match state.get_by_session(session_id) {
        Some(info) => {
            debug!(
                "🔍 [SSE_PROXY] 从内存获取项目信息: session_id={}, project_id={}",
                session_id,
                info.project_id()
            );
            info
        }
        None => {
            error!(
                "❌ [SSE_PROXY] 会话对应的项目信息不存在: session_id={}",
                session_id
            );
            return Err(create_error_response(
                StatusCode::NOT_FOUND,
                "SESSION_NOT_FOUND",
                "会话不存在或已过期。请重新发起请求。",
            ));
        }
    };

    let docker_manager = match docker_manager::global::get_global_docker_manager().await {
        Ok(dm) => dm,
        Err(e) => {
            error!("[SSE_PROXY] Failed to get global DockerManager: {}", e);
            return Err(create_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                "无法访问 Docker 服务，请联系管理员。",
            ));
        }
    };

    // ========== 阶段 2: 获取稳定的 container_name（不是 container_id） ==========
    // 🔧 关键修复：container_name 在容器重建后保持不变（如 computer-agent-runner-user_123）
    // 而 container_id 在每次容器重建后都会变化
    let container_name = match state.get_container_name_by_session(session_id) {
        Some(name) => {
            debug!(
                "🔍 [SSE_PROXY] 从 DuckDB 获取容器名称: session_id={}, container_name={}",
                session_id, name
            );
            name
        }
        None => {
            // 🔄 DuckDB 中没有记录，触发降级查询
            // 可能原因：
            // 1. 新 session 尚未写入 DuckDB（正常情况）
            // 2. 测试环境脏数据
            // 3. 容器重建后 DuckDB 未更新
            info!(
                "🔄 [SSE_PROXY] DuckDB 中未找到 session_id 记录，执行降级查询: session_id={}, project_id={}",
                session_id,
                project_info.project_id()
            );

            // 根据 service_type 选择不同的查询策略
            // （使用已获取的 docker_manager 和 project_info，避免重复获取）
            let resolved_container_name = match project_info.service_type() {
                Some(shared_types::ServiceType::ComputerAgentRunner) => {
                    // ComputerAgentRunner 模式：通过 user_id 查询容器
                    if let Some(user_id) = project_info.user_id() {
                        match docker_manager.get_user_container_info(&user_id).await {
                            Ok(Some(info)) => {
                                info!(
                                    "✅ [SSE_PROXY] 降级查询成功：通过 user_id 实时获取容器: user_id={}, container_name={}",
                                    user_id, info.container_name
                                );
                                info.container_name
                            }
                            Ok(None) => {
                                error!(
                                    "[SSE_PROXY] 降级Query failed：容器不存在: user_id={}",
                                    user_id
                                );
                                return Err(create_error_response(
                                    StatusCode::NOT_FOUND,
                                    "CONTAINER_NOT_FOUND",
                                    &format!("容器不存在: user_id={}", user_id),
                                ));
                            }
                            Err(e) => {
                                error!(
                                    "[SSE_PROXY] 降级Query failed：Failed to query container: {}",
                                    e
                                );
                                return Err(create_error_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "CONTAINER_ERROR",
                                    &format!("Failed to query container: {}", e),
                                ));
                            }
                        }
                    } else {
                        error!(
                            "[SSE_PROXY] ComputerAgentRunner 模式下缺少 user_id: session_id={}",
                            session_id
                        );
                        return Err(create_error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INVALID_DATA",
                            "项目缺少用户标识",
                        ));
                    }
                }
                _ => {
                    // RCoder 模式：从 project_info 获取容器名称，或使用 project_id 作为容器名称
                    //
                    // ⚠️ 注意：project_info 从 DuckDB 读取，可能包含部分过时数据
                    // - container_name: 稳定不变（容器重建后仍有效）
                    // - container_id, container_ip: 可能过时（容器重建后会变化）
                    //
                    // 阶段 3 会验证容器的真实存在性（通过内存信息或 Docker API）
                    // 因此即使 project_info.container() 为 None，也可以继续执行
                    match project_info.container() {
                        Some(container) => {
                            info!(
                                "✅ [SSE_PROXY] 降级查询成功：从 project_info(DuckDB) 获取容器名称: container_name={}",
                                container.container_name
                            );
                            container.container_name.clone()
                        }
                        None => {
                            // project_info 中没有容器信息，使用 project_id 作为容器名称
                            // 这通常发生在容器刚创建但尚未写入 DuckDB 的情况
                            // 阶段 3 会通过 Docker API 验证容器是否存在
                            warn!(
                                "⚠️ [SSE_PROXY] project_info 没有容器信息，使用 project_id 作为容器名称: project_id={}",
                                project_info.project_id()
                            );
                            project_info.project_id().to_string()
                        }
                    }
                }
            };

            resolved_container_name
        }
    };

    // ========== 阶段 3: 优先使用内存中的容器信息，避免不必要的 Docker API 调用 ==========

    // 🎯 优化策略：
    // 1. 首先检查内存中的 project_info.container() 是否已存在
    // 2. 如果存在 → 跳过 Docker API 调用（内存信息由创建逻辑保证最新）
    // 3. 如果不存在 → 调用 find_container_realtime 作为降级方案
    // 4. 后续会通过 gRPC GetStatus 进行最终健康检查
    if let Some(container) = project_info.container() {
        info!(
            "✅ [SSE_PROXY] 使用内存中的容器信息: container_name={}, container_ip={}",
            container.container_name, container.container_ip
        );
        // 内存中有容器信息，跳过 Docker API 检查
        // 后续会通过 gRPC GetStatus 进行健康检查
    } else {
        // 内存中没有容器信息，调用 Docker API 实时查询
        warn!(
            "⚠️ [SSE_PROXY] 内存中缺少容器信息，调用 Docker API 查询: container_name={}",
            container_name
        );
        match docker_manager
            .find_container_realtime(&container_name)
            .await
        {
            Ok(Some(result)) => {
                if result.is_running {
                    info!(
                        "✅ [SSE_PROXY] Docker API 查询成功，容器运行中: container_name={}",
                        container_name
                    );
                } else {
                    return Err(create_error_response(
                        StatusCode::NOT_FOUND,
                        "SESSION_EXPIRED",
                        "会话因不活动已被清理。请重新发起请求。",
                    ));
                }
            }
            Ok(None) => {
                error!(
                    "❌ [SSE_PROXY] 容器不存在: container_name={}",
                    container_name
                );
                return Err(create_error_response(
                    StatusCode::NOT_FOUND,
                    "SESSION_EXPIRED",
                    "容器不存在。请重新发起请求。",
                ));
            }
            Err(e) => {
                error!("❌ [SSE_PROXY] Docker API Query failed: {}", e);
                return Err(create_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                    "检查会话状态时出错，请稍后重试。",
                ));
            }
        }
    }

    // ========== 阶段 4: 返回验证通过的上下文 ==========

    // 🎯 优化：直接使用阶段 1 中已获取的 project_info，避免重复查询
    let project_id = project_info.project_id().to_string();

    // 注意：由于阶段 3 已经处理了 project_info.container() 为 None 的情况
    // （通过 Docker API 降级查询），这里无需再次验证容器信息的完整性
    info!(
        "✅ [SSE_PROXY] 所有验证通过: session_id={}, project_id={}, container_name={}",
        session_id, project_id, container_name
    );
    Ok((project_id, container_name))
}

/// 创建 SSE 响应流
///
/// 这个函数被 agent_session_notification 和 computer_agent_progress_notification 共同使用
/// 通过 container_name 创建 gRPC SSE 流
async fn build_sse_stream_from_container_name(
    container_name: String,
    session_id: String,
    project_id: String,
    grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
    container_ip_cache: Arc<crate::grpc::ContainerIpCache>,
    locale: &'static str,
    agent_type: &str, // 用于日志区分 "Agent" 或 "Computer Agent"
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    // Get latest container IP from Docker API in real-time（带缓存）
    // 使用 container_name（如 computer-agent-runner-user_123）查询
    // 因为 container_id 在容器重启后会改变，但 container_name 是稳定的
    let container_ip = match get_realtime_container_ip_with_cache(
        &container_name,
        &container_ip_cache,
        "", // 无 fallback_ip，直接使用 Docker API 查询结果
    )
    .await
    {
        Ok(ip) => {
            if !ip.is_empty() {
                info!(
                    "🔍 [gRPC_SSE] 获取容器 IP: container_name={}, ip={}",
                    container_name, ip
                );
                ip
            } else {
                error!(
                    "❌ [gRPC_SSE] 无法获取容器 IP: container_name={}",
                    container_name
                );
                return Err(create_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "GRPC_CONNECTION_ERROR",
                    "无法获取容器 IP 地址",
                ));
            }
        }
        Err(e) => {
            error!(
                "❌ [gRPC_SSE] 获取实时 IP 失败: container_name={}, error={}",
                container_name, e
            );
            return Err(create_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRPC_CONNECTION_ERROR",
                &format!("获取容器 IP 失败: {}", e),
            ));
        }
    };

    let grpc_addr = format!("{}:{}", container_ip, shared_types::GRPC_DEFAULT_PORT);
    info!(
        "🚀 [gRPC_SSE] 建立 {} gRPC SSE 代理连接: {}, project_id={}",
        agent_type, grpc_addr, project_id
    );

    // 创建 gRPC SSE 流
    let stream = crate::grpc::create_grpc_sse_stream(
        grpc_addr,
        session_id.clone(),
        project_id,
        grpc_pool.clone(),
        locale,
    )
    .await;

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
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
    params(
        SessionNotificationParams
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
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let locale = shared_types::current_request_locale();
    let session_id = &params.session_id;
    info!(
        "🔍 [SSE_PROXY] 收到SSE连接请求: session_id={:?}",
        session_id
    );

    // 使用核心验证函数获取上下文
    let (project_id, container_name) =
        validate_and_get_session_context(state.clone(), session_id).await?;

    // 使用通用函数创建 SSE 响应流
    build_sse_stream_from_container_name(
        container_name,
        session_id.to_string(),
        project_id,
        state.grpc_pool.clone(),
        state.container_ip_cache.clone(),
        locale,
        "Agent",
    )
    .await
}

#[utoipa::path(
    get,
    path = "/computer/agent/progress/{session_id}",
    params(
        SessionNotificationParams
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
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let locale = shared_types::current_request_locale();
    let session_id = &params.session_id;
    info!(
        "🔍 [SSE_PROXY] 收到 Computer Agent SSE连接请求: session_id={:?}",
        session_id
    );

    // 使用与 agent_session_notification 相同的验证逻辑
    let (project_id, container_name) =
        validate_and_get_session_context(state.clone(), session_id).await?;

    // 使用通用函数创建 SSE 响应流
    build_sse_stream_from_container_name(
        container_name,
        session_id.to_string(),
        project_id,
        state.grpc_pool.clone(),
        state.container_ip_cache.clone(),
        locale,
        "Computer Agent",
    )
    .await
}

/// 创建 SSE 代理流
async fn create_sse_proxy_stream(
    sse_url: String,
    session_id: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let (tx, rx) = tokio::sync::mpsc::channel(100);

    // 在后台任务中处理 SSE 连接
    tokio::spawn(async move {
        let client = Client::new();

        info!(
            "🔗 [SSE_PROXY] 开始连接容器SSE: url={}, session_id={}",
            sse_url, session_id
        );

        match client
            .get(&sse_url)
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    info!(
                        "✅ [SSE_PROXY] 成功连接到容器SSE: session_id={}",
                        session_id
                    );

                    let mut stream = response.bytes_stream();
                    let mut buffer = Vec::new();

                    while let Some(chunk_result) = stream.next().await {
                        match chunk_result {
                            Ok(chunk) => {
                                buffer.extend_from_slice(&chunk);

                                // 按双换行符分割 SSE 事件
                                while let Some(event_end) =
                                    buffer.windows(2).position(|w| w == [b'\n', b'\n'])
                                {
                                    let event_data = buffer[..event_end].to_vec();
                                    buffer = buffer[event_end + 2..].to_vec();

                                    if !event_data.is_empty() {
                                        debug!(
                                            "📨 [SSE_PROXY] 透传SSE事件: session_id={}, event_len={}",
                                            session_id,
                                            event_data.len()
                                        );

                                        // 直接透传原始 SSE 数据
                                        if let Ok(event_text) = String::from_utf8(event_data)
                                            && let Some(event) =
                                                create_passthrough_event(&event_text)
                                            && tx.send(Ok(event)).await.is_err()
                                        {
                                            warn!(
                                                "⚠️ [SSE_PROXY] 客户端已断开连接: session_id={}",
                                                session_id
                                            );
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    "❌ [SSE_PROXY] 读取SSE流失败: session_id={}, error={}",
                                    session_id, e
                                );
                                break;
                            }
                        }
                    }
                } else {
                    error!(
                        "❌ [SSE_PROXY] 容器SSE连接失败: session_id={}, status={}",
                        session_id,
                        response.status()
                    );

                    // 发送错误事件
                    let error_event = Event::default()
                        .event("error")
                        .data(format!("容器连接失败: {}", response.status()));
                    if let Err(send_err) = tx.send(Ok(error_event)).await {
                        warn!(
                            "⚠️ [SSE_PROXY] 发送错误事件失败: session_id={}, error={}",
                            session_id, send_err
                        );
                    }
                }
            }
            Err(e) => {
                error!(
                    "❌ [SSE_PROXY] 无法连接到容器SSE: session_id={}, error={}",
                    session_id, e
                );

                // 发送连接错误事件
                let error_event = Event::default()
                    .event("error")
                    .data(format!("连接错误: {}", e));
                if let Err(send_err) = tx.send(Ok(error_event)).await {
                    warn!(
                        "⚠️ [SSE_PROXY] 发送错误事件失败: session_id={}, error={}",
                        session_id, send_err
                    );
                }
            }
        }

        info!(
            "[SSE_PROXY] SSEproxyconnection message : session_id={}",
            session_id
        );
    });

    ReceiverStream::new(rx)
}

/// 创建透传 SSE 事件
///
/// 正确解析SSE消息的各个部分，避免重复的data:前缀
fn create_passthrough_event(event_text: &str) -> Option<Event> {
    let mut event_type = None;
    let mut data_lines = Vec::new();

    // 解析SSE消息的各个部分
    for line in event_text.lines() {
        if line.starts_with("event:") {
            event_type = Some(line[6..].trim().to_string());
        } else if line.starts_with("data:") {
            data_lines.push(line[5..].trim());
        }
    }

    // 只有当有数据内容时才创建事件
    if !data_lines.is_empty() {
        let data_content = data_lines.join("\n");
        let mut event = Event::default().data(data_content);

        // 如果有事件类型，则设置事件类型
        if let Some(event_type) = event_type {
            event = event.event(event_type);
        }

        Some(event)
    } else {
        None
    }
}

/// 创建错误响应
fn create_error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let locale = shared_types::current_request_locale();
    let mapped_code = map_error_code_for_locale(code);
    let localized_message = shared_types::get_error_message(mapped_code, locale);
    let error_body = HttpResult::<()>::error(code, &localized_message);
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

/// 获取容器的 SSE 端点 URL
async fn get_container_sse_url(
    project_id: &str,
    _agent_info: &ProjectAndContainerInfo,
    session_id: &str,
) -> Result<String, AppError> {
    info!(
        "🔍 [CONTAINER] 获取容器SSE端点: project_id={}, session_id={}",
        project_id, session_id
    );

    // 🎯 修复：使用全局DockerManager实例
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("[CONTAINER] Failed to get global DockerManager: {}", e);
            AppError::internal_server_error(&format!("Failed to get global DockerManager: {}", e))
        })?;

    // 使用高级 API 获取容器信息
    if let Some(info) = docker_manager
        .get_agent_info(project_id)
        .await
        .map_err(|e| {
            error!("[CONTAINER] Failed to get container info: {}", e);
            AppError::internal_server_error(&format!("Failed to get container info: {}", e))
        })?
    {
        // 构建 SSE 端点 URL
        // info.service_url 格式为 http://ip:8086
        let sse_url = format!("{}/agent/progress/{}", info.service_url, session_id);

        info!("[CONTAINER] getcontainerSSE message : {}", sse_url);
        Ok(sse_url)
    } else {
        Err(AppError::internal_server_error("Container info not found"))
    }
}
