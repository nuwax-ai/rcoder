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
use utoipa::{IntoParams, ToSchema};

/// 会话通知路径参数
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    #[param(example = "session456")]
    pub session_id: String,
}

/// 核心验证函数：验证会话并获取容器信息
///
/// 这个函数被 SSE 通知处理器和文档生成器共同使用
/// 执行所有必要的验证和查找逻辑，但不执行实际的消息流创建
async fn validate_and_get_session_context(
    state: Arc<crate::router::AppState>,
    session_id: &str,
) -> Result<(String, Arc<shared_types::ProjectAndContainerInfo>, String), Response> {
    // 阶段 2.3: 容器存在性预检
    let container_id = match state.session_to_container_id.get(session_id) {
        Some(cid) => cid.value().clone(),
        None => {
            warn!(
                "❌ [SSE_PROXY] 在 session_to_container_id 映射中未找到会话: session_id={}",
                session_id
            );
            return Err(create_error_response(
                StatusCode::NOT_FOUND,
                "SESSION_NOT_FOUND",
                "会话不存在或已过期。请重新发起请求。",
            ));
        }
    };

    // 检查容器是否仍在运行
    let docker_manager = match docker_manager::global::get_global_docker_manager().await {
        Ok(dm) => dm,
        Err(e) => {
            error!("❌ [SSE_PROXY] 获取全局 DockerManager 失败: {}", e);
            return Err(create_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "无法访问 Docker 服务，请联系管理员。",
            ));
        }
    };

    match docker_manager.is_container_running(&container_id).await {
        Ok(true) => {
            info!(
                "✅ [SSE_PROXY] 容器检查通过: container_id={}, 状态=运行中",
                container_id
            );
            // 容器正在运行，继续执行
        }
        Ok(false) => {
            error!("❌ [SSE_PROXY] 容器已停止: container_id={}", container_id);
            // 清理陈旧的会话条目
            state.session_to_container_id.remove(session_id);
            return Err(create_error_response(
                StatusCode::NOT_FOUND,
                "SESSION_EXPIRED",
                "会话因不活动已被清理。请重新发起请求。",
            ));
        }
        Err(e) => {
            error!(
                "❌ [SSE_PROXY] 检查容器状态失败: container_id={}, error={}",
                container_id, e
            );
            return Err(create_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "检查会话状态时出错，请稍后重试。",
            ));
        }
    };

    // 容器验证通过后，查找对应的项目和代理信息
    match find_container_by_session_id(&state, &session_id) {
        Some((project_id, agent_info)) => {
            info!(
                "✅ [SSE_PROXY] 找到项目: session_id={}, project_id={}",
                session_id, project_id
            );

            // 🎯 直接从 agent_info 中获取容器 IP 构建 gRPC 地址
            // 对于 Computer Agent，容器信息已经在 ProjectAndContainerInfo 中
            match agent_info.container() {
                Some(_) => {
                    // 验证通过，返回上下文信息
                    Ok((project_id, agent_info, container_id))
                }
                None => {
                    error!(
                        "❌ [gRPC_SSE] ProjectAndContainerInfo 中没有容器信息: session_id={}, project_id={}",
                        session_id, project_id
                    );

                    Err(create_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "GRPC_CONNECTION_ERROR",
                        "会话中缺少容器信息，请重新发起请求。",
                    ))
                }
            }
        }
        None => {
            // 理论上在预检后不应该发生，但作为保障
            error!(
                "❌ [SSE_PROXY] 状态不一致：预检通过但在 project_and_agent_map 中未找到: session_id={}",
                session_id
            );

            Err(create_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INCONSISTENT_STATE",
                "会话状态不一致，请重新发起请求。",
            ))
        }
    }
}

/// 创建 SSE 响应流
///
/// 这个函数被 agent_session_notification 和 computer_agent_progress_notification 共同使用
/// 负责从代理信息中创建 gRPC SSE 流
async fn build_sse_stream_from_agent_info(
    agent_info: Arc<shared_types::ProjectAndContainerInfo>,
    session_id: String,
    grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
    agent_type: &str, // 用于日志区分 "Agent" 或 "Computer Agent"
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    // 🎯 直接从 agent_info 中获取容器 IP 构建 gRPC 地址
    match agent_info.container() {
        Some(container) => {
            let grpc_addr = format!(
                "{}:{}",
                container.container_ip,
                shared_types::GRPC_DEFAULT_PORT
            );
            info!(
                "🚀 [gRPC_SSE] 建立 {} gRPC SSE 代理连接: {}",
                agent_type, grpc_addr
            );

            // 创建 gRPC SSE 流
            let stream = crate::grpc::create_grpc_sse_stream(
                grpc_addr,
                session_id.clone(),
                grpc_pool.clone(),
            )
            .await;

            Ok(Sse::new(stream).keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text("keep-alive"),
            ))
        }
        None => {
            // 理论上在 validate_and_get_session_context 中已经验证过
            Err(create_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "GRPC_CONNECTION_ERROR",
                "会话中缺少容器信息，请重新发起请求。",
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
    let session_id = &params.session_id;
    info!(
        "🔍 [SSE_PROXY] 收到SSE连接请求: session_id={:?}",
        session_id
    );

    // 使用核心验证函数获取上下文
    let (_project_id, agent_info, _container_id) =
        validate_and_get_session_context(state.clone(), session_id).await?;

    // 使用通用函数创建 SSE 响应流
    build_sse_stream_from_agent_info(
        agent_info,
        session_id.to_string(),
        state.grpc_pool.clone(),
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
    tag = "computer",
    operation_id = "computer_agent_progress_notification",
    summary = "Computer Agent 专用会话 SSE 通知流",
    description = "为 Computer Agent 专用的进度流接口，建立 SSE 连接实时接收执行进度和状态更新。此接口与 `/computer/progress/{session_id}` 功能相同，提供更明确的路径结构。\n\n## 🔄 核心逻辑\n\n该接口与 `agent_session_notification` 使用相同的数据验证和查找逻辑：\n\n1. 验证会话ID对应的容器是否存在\n2. 检查容器是否正在运行\n3. 查找对应的项目和代理信息\n4. 建立 gRPC SSE 连接\n\n所有验证逻辑都通过 `validate_and_get_session_context` 函数统一处理。"
)]
pub async fn computer_agent_progress_notification(
    Path(params): Path<SessionNotificationParams>,
    State(state): State<Arc<crate::router::AppState>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let session_id = &params.session_id;
    info!(
        "🔍 [SSE_PROXY] 收到 Computer Agent SSE连接请求: session_id={:?}",
        session_id
    );

    // 使用与 agent_session_notification 相同的验证逻辑
    let (_project_id, agent_info, _container_id) =
        validate_and_get_session_context(state.clone(), session_id).await?;

    // 使用通用函数创建 SSE 响应流
    build_sse_stream_from_agent_info(
        agent_info,
        session_id.to_string(),
        state.grpc_pool.clone(),
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

    // 🎯 修复：使用全局DockerManager实例
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER] 获取全局 DockerManager 失败: {}", e);
            AppError::internal_server_error(&format!("获取全局 DockerManager 失败: {}", e))
        })?;

    // 使用高级 API 获取容器信息
    if let Some(info) = docker_manager
        .get_agent_info(project_id)
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER] 获取容器信息失败: {}", e);
            AppError::internal_server_error(&format!("获取容器信息失败: {}", e))
        })?
    {
        // 构建 SSE 端点 URL
        // info.service_url 格式为 http://ip:8086
        let sse_url = format!("{}/agent/progress/{}", info.service_url, session_id);

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
        return Some((project_info.project_id().to_string(), project_info.clone()));
    }

    // 如果 sessions 中没找到，遍历 project_and_agent_map 查找
    for entry in state.project_and_agent_map.iter() {
        let agent_info = entry.value();
        if let Some(agent_session_id) = agent_info.session_id()
            && agent_session_id == session_id
        {
            return Some((entry.key().clone(), agent_info.clone()));
        }
    }
    None
}
