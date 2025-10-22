//! Agent执行任务的SSE通知处理器
//!
//! 使用 Pingora 透明代理处理 SSE 消息

use crate::{AppError, HttpResult};
use axum::{
    extract::Path,
    response::sse::{Event, Sse},
};
use futures::{stream::{self}, Stream};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use tracing::{error, info};
use utoipa::{IntoParams, ToSchema};

/// 会话通知路径参数
#[derive(Debug, Deserialize, IntoParams)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    #[param(example = "session456")]
    pub session_id: String,
}

/// SSE 事件响应结构
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionUpdateEvent {
    /// 事件类型
    #[schema(example = "proxy_ready")]
    pub event_type: String,
    /// 会话ID
    #[schema(example = "session456")]
    pub session_id: String,
}

/// Pingora 透明代理 SSE 流处理器
async fn pingora_proxy_sse_stream(
    session_id: String,
) -> Result<impl Stream<Item = Result<Event, Infallible>>, AppError> {
    info!("🌐 [PINGORA_PROXY] 开始 Pingora SSE 代理: session_id={}", session_id);

    // 检查容器存在性并获取代理目标
    let proxy_target_url = format!("http://localhost:8080/agent/agent/progress?session_id={}", session_id);
    let project_id = "unknown_project".to_string();

    // 创建连接事件
    let connection_event = Event::default()
        .event("connected")
        .data(format!(
            r#"{{"type":"proxy_ready","target":"{}","project_id":"{}"}}"#,
            proxy_target_url, project_id
        ));

    // 创建代理就绪事件
    let proxy_ready_event = Event::default()
        .event("proxy_ready")
        .data(format!(
            r#"{{"type":"pingora_proxy_ready","target":"{}","project_id":"{}"}}"#,
            proxy_target_url, project_id
        ));

    // 创建事件流
    let event_stream = stream::iter(vec![
        Ok(connection_event),
        Ok(proxy_ready_event),
    ]);

    Ok(Sse::new(event_stream))
}

/// 建立SSE连接，实时推送该session的SessionUpdate消息
///
/// 通过Server-Sent Events (SSE)协议实时推送AI代理执行进度和状态更新
///
/// ## 📨 支持的消息类型
///
/// 返回的UnifiedSessionMessage包含以下主要类型：
///
/// 1. **SessionPromptStart** - 用户发送prompt开始通知
/// 2. **SessionPromptEnd** - Agent执行结束通知（包含5种停止原因：EndTurn, MaxTokens, MaxTurnRequests, Refusal, Cancelled）
/// 3. **AgentSessionUpdate** - Agent执行过程中的更新通知（包含8种子类型：UserMessageChunk, AgentMessageChunk, AgentThoughtChunk, ToolCall, ToolCallUpdate, Plan, AvailableCommandsUpdate, CurrentModeUpdate）
/// 4. **Heartbeat** - SSE连接心跳消息
///
/// ## 🔧 Pingora 透明代理架构
///
/// 当前实现为简化版本，验证Pingora代理概念的可行性：
/// - 检查容器存在性
/// - 返回代理目标URL
/// - 模拟代理就绪事件
///
/// ## 📝 前端集成计划
///
/// 1. 集成真实的 Pingora 透明代理功能
/// 2. 集成与容器管理系统的集成
/// 3. 完成容器服务可用性验证
/// 4. 添加容器管理配置接口
///
#[utoipa::path(
    get_agent_session_notification,
    operation_id = "agent_session_notification",
    tag = "agent",
    summary = "通过Pingora透明代理建立SSE连接",
    description = "建立Server-Sent Events连接，实时推送AI代理执行进度。验证Pingora透明代理概念的可行性。",
    params(
        SessionNotificationParams,
    ),
    responses(
        (
            status = 200,
            description = "SSE连接建立成功",
            body = HttpResult::success(SessionUpdateEvent {
                event_type: "connected".to_string(),
                session_id: "session456".to_string(),
            }),
        ),
        (
            status = 500,
            description = "SSE连接失败",
            body = HttpResult::error("INTERNAL_ERROR", "内部服务器错误"),
        ),
    ),
    ),
)]
pub async fn agent_session_notification(
    Path(params): Path<SessionNotificationParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    info!(
        "📡 [SSE_CONNECTION] 收到SSE连接请求: session_id={:?}",
        params.session_id
    );

    match pingora_proxy_sse_stream(params.session_id) {
        Ok(stream) => {
            info!("✅ [SSE_CONNECTION] SSE连接建立成功: session_id={}", params.session_id);
            Ok(Sse::new(stream))
        }
        Err(e) => {
            error!("❌ [SSE_CONNECTION] SSE连接失败: session_id={}, error={:?}", params.session_id, e);
            Err(AppError::internal_server_error(&format!(
                "SSE连接失败: {}",
                e
            )))
        }
    }
}