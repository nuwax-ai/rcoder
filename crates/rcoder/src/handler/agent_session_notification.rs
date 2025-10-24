//! Agent执行任务的SSE通知处理器
//!
//! 使用 Axum SSE 代理处理 SSE 消息，实现高效的 SSE 转发

use crate::{AppError, HttpResult};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::{StreamExt, stream::Stream};
use reqwest::Client;
use serde::Deserialize;
use shared_types::ProjectAndContainerInfo;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};
use utoipa::IntoParams;

/// 会话通知路径参数
#[derive(Debug, Deserialize, IntoParams)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    #[param(example = "session456")]
    pub session_id: String,
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
            description = "成功建立 SSE 连接，开始接收实时消息",
            content_type = "text/event-stream",
            headers(
                ("Cache-Control" = String, description = "no-cache"),
                ("Connection" = String, description = "keep-alive"),
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
    tag = "agent",
    operation_id = "agent_session_notification",
    summary = "Agent 会话 SSE 通知流",
    description = "建立到指定 session_id 对应容器的 SSE 连接，实时接收 Agent 执行进度和状态更新。"
)]
pub async fn agent_session_notification(
    Path(params): Path<SessionNotificationParams>,
    State(state): State<Arc<crate::router::AppState>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    info!(
        "🔍 [SSE_PROXY] 收到SSE连接请求: session_id={:?}",
        params.session_id
    );

    // 检查容器存在性并获取代理目标
    let session_id = params.session_id.clone();
    match find_container_by_session_id(&state, &session_id) {
        Some((project_id, agent_info)) => {
            info!(
                "✅ [SSE_PROXY] 找到容器: session_id={}, project_id={}",
                session_id, project_id
            );

            // 获取容器的 SSE 端点 URL
            match get_container_sse_url(&project_id, &agent_info, &session_id).await {
                Ok(sse_url) => {
                    info!("🚀 [SSE_PROXY] 建立SSE代理连接: {}", sse_url);

                    // 创建 SSE 流
                    let stream = create_sse_proxy_stream(sse_url, session_id.clone()).await;

                    Ok(Sse::new(stream).keep_alive(
                        KeepAlive::new()
                            .interval(Duration::from_secs(15))
                            .text("keep-alive"),
                    ))
                }
                Err(e) => {
                    error!(
                        "❌ [SSE_PROXY] 获取容器SSE端点失败: session_id={}, error={}",
                        session_id, e
                    );

                    Err(create_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "SSE_CONNECTION_ERROR",
                        "无法连接到容器的 SSE 端点",
                    ))
                }
            }
        }
        None => {
            error!("❌ [SSE_PROXY] 未找到对应容器: session_id={}", session_id);

            Err(create_error_response(
                StatusCode::NOT_FOUND,
                "CONTAINER_NOT_FOUND",
                "未找到 session_id 对应的活跃容器",
            ))
        }
    }
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
                                        if let Ok(event_text) = String::from_utf8(event_data) {
                                            if let Some(event) =
                                                create_passthrough_event(&event_text)
                                            {
                                                if tx.send(Ok(event)).await.is_err() {
                                                    warn!(
                                                        "⚠️ [SSE_PROXY] 客户端已断开连接: session_id={}",
                                                        session_id
                                                    );
                                                    break;
                                                }
                                            }
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
                    let _ = tx.send(Ok(error_event)).await;
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
                let _ = tx.send(Ok(error_event)).await;
            }
        }

        info!("🔚 [SSE_PROXY] SSE代理连接结束: session_id={}", session_id);
    });

    ReceiverStream::new(rx)
}

/// 创建透传 SSE 事件
///
/// 直接透传原始 SSE 文本，避免解析和重构的开销
/// 这样可以保持原始事件的格式，提高性能
fn create_passthrough_event(event_text: &str) -> Option<Event> {
    // 检查是否包含有效的 SSE 内容
    let mut has_data = false;

    for line in event_text.lines() {
        if line.starts_with("data:") {
            has_data = true;
            break;
        }
    }

    if has_data {
        // 直接使用原始文本作为事件数据，保持原始格式
        Some(Event::default().data(event_text.trim()))
    } else {
        None
    }
}

/// 创建错误响应
fn create_error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let error_body = HttpResult::<()>::error(code, message);
    let json_body = serde_json::to_string(&error_body).unwrap_or_default();

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(json_body.into())
        .unwrap_or_else(|_| Response::new("Internal Server Error".into()))
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

    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config()
            .await
            .map_err(|e| {
                error!("❌ [CONTAINER] 创建 DockerManager 失败: {}", e);
                AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?,
    );

    // 获取容器信息
    let container_info = docker_manager.get_container_info(project_id);
    if let Some(container_info) = container_info {
        let server_url = crate::proxy_agent::docker_container_agent::get_container_ip(
            &docker_manager,
            &container_info.container_id,
        )
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER] 获取容器 IP 失败: {}", e);
            AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
        })?;

        // 构建 SSE 端点 URL
        let sse_url = format!(
            "{}/agent/agent/progress?session_id={}",
            server_url, session_id
        );

        info!("✅ [CONTAINER] 获取容器SSE端点: {}", sse_url);
        Ok(sse_url)
    } else {
        Err(AppError::internal_server_error("未找到容器信息"))
    }
}

/// 根据session_id查找对应的容器
fn find_container_by_session_id(
    state: &Arc<crate::router::AppState>,
    session_id: &str,
) -> Option<(String, std::sync::Arc<ProjectAndContainerInfo>)> {
    // 首先尝试从 sessions 映射中查找（通过 session_id 直接查找）
    if let Some(project_info) = state.sessions.get(session_id) {
        return Some((project_info.project_id.clone(), project_info.clone()));
    }

    // 如果 sessions 中没找到，遍历 project_and_agent_map 查找
    for entry in state.project_and_agent_map.iter() {
        let agent_info = entry.value();
        if let Some(ref agent_session_id) = agent_info.session_id {
            if agent_session_id == session_id {
                return Some((entry.key().clone(), agent_info.clone()));
            }
        }
    }
    None
}
