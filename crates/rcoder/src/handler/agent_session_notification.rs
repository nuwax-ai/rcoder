//! Agent执行任务的SSE通知处理器
//!
//! 转发SSE请求到容器内的 agent_runner 服务

use crate::{AppError, model::HttpResult, model::UnifiedSessionMessage};
use axum::{
    extract::Path,
    response::sse::{Event, Sse},
};
use futures::stream::{self, Stream};
use serde::Deserialize;
use serde::Serialize;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
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
    #[schema(example = "prompt_start")]
    pub event_type: String,
    /// 会话ID
    #[schema(example = "session456")]
    pub session_id: String,
    /// 统一会话消息
    pub message: UnifiedSessionMessage,
}

/// 转发SSE请求到容器内的 agent_runner 服务
async fn forward_sse_request_to_container(
    session_id: &str,
) -> Result<impl Stream<Item = Result<Event, Infallible>>, AppError> {
    info!(
        "🚀 [SSE_FORWARD] 开始转发SSE请求: session_id={}",
        session_id
    );

    // 对于SSE连接，我们需要先确保容器存在（可能需要创建）
    // 但SSE通常表示已有会话，所以我们优先查找现有容器
    let (server_url, _project_id_final) = ensure_container_exists_for_sse(session_id).await?;

    // 创建客户端连接到容器内的 agent/agent/progress 接口
    let client = reqwest::Client::new();
    let progress_url = format!("{}/agent/agent/progress?session_id={}", server_url, session_id);

    info!(
        "📤 [SSE_FORWARD] 连接到容器SSE接口: {}",
        progress_url
    );

    // 创建到容器的SSE连接
    let response = client
        .get(&progress_url)
        .send()
        .await
        .map_err(|e| {
            error!("❌ [SSE_FORWARD] 连接容器SSE接口失败: {}", e);
            AppError::internal_server_error(&format!("连接容器SSE接口失败: {}", e))
        })?;

    if response.status().is_success() {
        let container_event_stream = response
            .bytes_stream()
            .map_err(|e| {
                error!("❌ [SSE_FORWARD] 创建容器事件流失败: {}", e);
                AppError::internal_server_error(&format!("创建容器事件流失败: {}", e))
            })?;

        // 将容器的事件流转换为我们的SSE事件格式
        let event_stream = container_event_stream
            .map(|result| {
                match result {
                    Ok(chunk) => {
                        // 解析容器返回的事件数据并转换为我们的格式
                        match parse_container_sse_event(&chunk) {
                            Some(event_data) => {
                                Ok(Event::default()
                                    .event(event_data.event_type.clone())
                                    .data(serde_json::to_string(&event_data.message).unwrap_or_else(|_| "{}".to_string())))
                            },
                            None => {
                                // 如果无法解析，跳过或创建心跳事件
                                Ok(Event::default()
                                    .event("heartbeat")
                                    .data(r#"{"type":"heartbeat","message":"keep-alive"}"#))
                            }
                        }
                    }
                    Err(e) => {
                        error!("❌ [SSE_FORWARD] 处理容器事件失败: {}", e);
                        Err(Infallible::new(e))
                    }
                }
            });

        info!("✅ [SSE_FORWARD] 容器SSE连接建立成功: session_id={}", session_id);
        Ok(event_stream)
    } else {
        error!(
            "❌ [SSE_FORWARD] 容器SSE连接失败: status={}, body={:?}",
            response.status(),
            response.text().await
        );

        // 如果连接失败，返回一个错误事件流
        let error_event_stream = stream::once(Ok(Event::default()
            .event("error")
            .data(r#"{"type":"error","message":"Failed to connect to container SSE"}"#)));

        Ok(error_event_stream)
    }
}

/// 解析容器返回的SSE事件数据
fn parse_container_sse_event(chunk: &[u8]) -> Option<ContainerSseEvent> {
    // 简化的事件解析逻辑
    let chunk_str = String::from_utf8(chunk).ok()?;

    // 尝试解析JSON格式的SSE数据
    if let Ok(event_data) = serde_json::from_str::<ContainerSseEventData>(&chunk_str) {
        return Some(ContainerSseEvent {
            event_type: event_data.event_type,
            message: event_data.message,
        });
    }

    None
}

/// 容器SSE事件数据格式
#[derive(Debug, Deserialize)]
struct ContainerSseEventData {
    pub event_type: String,
    pub message: UnifiedSessionMessage,
}

/// 容器SSE事件
#[derive(Debug)]
struct ContainerSseEvent {
    pub event_type: String,
    pub message: UnifiedSessionMessage,
}

/// 检查或创建容器（用于SSE请求）
async fn ensure_container_exists_for_sse(session_id: &str) -> Result<(String, String), AppError> {
    // 对于SSE请求，我们优先查找现有的容器
    // 如果找不到，可能需要创建一个容器来处理这个session
    if let Some((project_id, agent_info)) = find_container_by_session_id(session_id) {
        info!("🔍 [SSE_FORWARD] 找到现有容器: project_id={}, session_id={}", project_id, session_id);

        let docker_manager = std::sync::Arc::new(
            docker_manager::DockerManager::with_default_config().await
                .map_err(|e| {
                    error!("❌ [SSE_FORWARD] 创建 DockerManager 失败: {}", e);
                    AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
                })?
        );

        // 获取容器 IP 地址
        let container_info = docker_manager.get_container_info(&project_id);
        if let Some(container_info) = container_info {
            let server_url = crate::proxy_agent::docker_container_agent::get_container_ip(&docker_manager, &container_info.container_id, container_info.assigned_port).await
                .map_err(|e| {
                    error!("❌ [SSE_FORWARD] 获取容器 IP 失败: {}", e);
                    AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
                })?;

            info!("✅ [SSE_FORWARD] 获取容器服务 URL: {}", server_url);
            Ok((server_url, project_id))
        } else {
            Err(AppError::internal_server_error("未找到容器信息"))
        }
    } else {
        // 没有找到现有容器，为SSE请求创建一个临时容器
        info!("🏗️ [SSE_FORWARD] 未找到容器，为SSE请求创建临时容器: session_id={}", session_id);

        // 使用默认配置创建临时容器用于SSE
        let chat_prompt = crate::model::ChatPromptBuilder::default()
            .project_id("sse_temp".to_string())
            .session_id(session_id.to_string())
            .prompt("sse_request".to_string())
            .build()
            .map_err(|e| {
                error!("❌ [SSE_FORWARD] 构建 ChatPrompt 失败: {}", e);
                AppError::internal_server_error(&format!("构建 ChatPrompt 失败: {}", e))
            })?;

        // 创建临时容器
        create_container_for_sse(&chat_prompt, None).await?;

        // 返回错误，因为SSE不应该需要创建新容器
        Err(AppError::internal_server_error("SSE request requires existing session"))
    }
}

/// 根据session_id查找对应的容器
fn find_container_by_session_id(session_id: &str) -> Option<(String, std::sync::Arc<crate::model::ProjectAndAgentInfo>)> {
    use crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP;

    for entry in PROJECT_AND_AGENT_INFO_MAP.iter() {
        let agent_info = entry.value();
        if agent_info.session_id.to_string() == session_id {
            return Some((entry.key().clone(), agent_info.clone()));
        }
    }
    None
}

/// 为SSE请求创建容器
async fn create_container_for_sse(
    chat_prompt: &crate::model::ChatPrompt,
    _model_provider: Option<shared_types::ModelProviderConfig>,
) -> Result<(), AppError> {
    let project_id = &chat_prompt.project_id;
    info!("🏗️ [SSE_FORWARD] 开始为SSE请求创建容器: project_id={}", project_id);

    // 使用 docker_container_agent 创建容器
    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config().await
            .map_err(|e| {
                error!("❌ [SSE_FORWARD] 创建 DockerManager 失败: project_id={}, error={}", project_id, e);
                AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?
    );

    let connection_info = crate::proxy_agent::docker_container_agent::start_docker_container_agent_service(
        chat_prompt.clone(),
        None, // SSE请求不需要特定的 model provider
        docker_manager,
    ).await.map_err(|e| {
        error!("❌ [SSE_FORWARD] 创建容器失败: project_id={}, error={}", project_id, e);
        AppError::internal_server_error(&format!("创建容器失败: {}", e))
    })?;

    info!("✅ [SSE_FORWARD] 容器创建成功: project_id={}, session_id={}",
          project_id, connection_info.session_id);

    // 创建生命周期守卫并存储到 MAP 中
    let project_and_agent_info = crate::model::ProjectAndAgentInfo {
        project_id: project_id.clone(),
        session_id: connection_info.session_id.clone(),
        prompt_tx: connection_info.prompt_tx.clone(),
        cancel_tx: connection_info.cancel_tx.clone(),
        model_provider: None,
        request_id: chat_prompt.request_id.clone(),
        status: crate::model::AgentStatus::Idle,
        last_activity: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
        lifecycle_guard: connection_info.stop_handle
            .ok_or_else(|| AppError::internal_server_error("缺少生命周期守卫"))?
            .as_ref().clone(),
    };

    // 存储到全局 MAP
    crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP.insert(project_id.clone(), project_and_agent_info);

    // 建立 project_id -> session_id 映射
    let session_id_str = connection_info.session_id.to_string();
    let cleared_old = crate::service::session_cache::ensure_project_session(project_id, &session_id_str).await;
    if cleared_old > 0 {
        info!("🧹 Project session 映射更新，已清理旧消息: project_id={}, cleared_count={}",
              project_id, cleared_old);
    }

    info!("✅ [SSE_FORWARD] 容器创建完成并已注册: project_id={}", project_id);
    Ok(())
}

/// 建立SSE连接，将请求转发到容器内的 agent_runner 服务
///
/// 通过容器内的 agent_runner/agent/progress 接口获取实时会话更新
#[utoipa::path(
    get,
    path = "/agent/progress/{session_id}",
    params(
        SessionNotificationParams
    ),
    responses(
        (
            status = 200,
            description = "成功建立SSE连接，开始转发容器事件",
            content_type = "text/event-stream",
            body = Sse<impl Stream<Item = Result<Event, Infallible>>>,
            example = description("返回SSE事件流")
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "INVALID_PARAMS",
                    "message": "Invalid session_id parameter"
                }
            })
        ),
        (
            status = 500,
            description = "转发SSE请求失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "SSE_FAILED",
                    "message": "Failed to forward SSE request to container"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_session_notification",
    summary = "转发Agent会话通知",
    description = "建立SSE连接并将事件请求转发到容器内的 agent_runner/agent/progress 接口，通过容器获取实时会话更新事件。"
)]
#[instrument()]
pub async fn agent_session_notification(
    Path(params): Path<SessionNotificationParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    info!(
        "📡 [SSE_FORWARD] 收到SSE连接请求: session_id={:?}",
        params.session_id
    );

    // 直接转发到容器内的 agent_runner 服务
    let event_stream = forward_sse_request_to_container(&params.session_id).await?;

    // 添加心跳和超时处理
    let session_id = params.session_id.clone();
    let event_stream_with_heartbeat = event_stream
        .merge(stream::unfold(
            (true, session_id, CancellationToken::new()),
            move |(state, _session_id, _cancel_token)| async move {
                let mut interval = tokio::time::interval(Duration::from_secs(30));

                loop {
                    tokio::select! {
                        _ = cancel_token.cancelled() => {
                            info!("🔌 [SSE_FORWARD] SSE连接被取消: session_id={}", _session_id);
                            break None;
                        }
                        _ = interval.tick() => {
                            if state {
                                break Some(Event::default()
                                    .event("heartbeat")
                                    .data(r#"{"type":"heartbeat","message":"keep-alive","timestamp":"".to_string()+r#"}"#));
                            } else {
                                break None;
                            }
                        }
                    }
                }
            },
        ),
    );

    info!("✅ [SSE_FORWARD] SSE转发连接建立: session_id={}", session_id);
    Ok(Sse::new(event_stream_with_heartbeat))
}