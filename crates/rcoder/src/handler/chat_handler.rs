//! 聊天处理器
//!
//! 将原始 HTTP 请求直接转发到容器内的 agent_runner 服务

use anyhow::Result;
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use shared_types::ModelProviderConfig;
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, error, info, instrument};
use utoipa::ToSchema;
use uuid::Uuid;
use reqwest::Client;

use crate::proxy_agent::{PROJECT_AND_AGENT_INFO_MAP, docker_container_agent};
use crate::service::session_cache::{SESSION_CACHE, PROJECT_SESSION_MAP};
use crate::{*, router::AppState};
use crate::utils::prompt_builder::PromptBuilder;
use shared_types::{ChatPromptBuilder, AgentType};

/// 用户请求结构 - 支持多媒体内容
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct ChatRequest {
    /// 用户输入的 prompt
    #[schema(example = "帮我写一个 Rust 的 Hello World 程序")]
    pub prompt: String,
    /// 可选的项目 ID
    #[schema(example = "test_project")]
    pub project_id: Option<String>,
    /// 可选的会话 ID，如果不提供则创建新会话
    #[schema(example = "session456")]
    pub session_id: Option<String>,
    /// 可选的附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// 数据源附件列表 - 用于AI开发时获取外部数据源信息（如API接口、数据库等）
    /// 直接传递 JSON 字符串数组，简化使用方式
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_source_attachments: Vec<String>,
    /// 模型配置
    #[schema(
        example = json!({
            "id": "openai_gpt4",
            "name": "openai",
            "base_url": "https://api.openai.com/v1",
            "api_key": "sk-...",
            "requires_openai_auth": true,
            "default_model": "gpt-4",
            "api_protocol": "openai"
        })
    )]
    pub model_provider: Option<ModelProviderConfig>,
    /// 可选的请求ID，如果不提供则自动生成，用于标识和追踪请求
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "req_123456789")]
    pub request_id: Option<String>,
}


/// 生成不带中划线的随机项目ID
fn generate_project_id() -> String {
    Uuid::new_v4().to_string().replace("-", "")
}

/// 生成不带中划线的随机请求ID
fn generate_request_id() -> String {
    Uuid::new_v4().to_string().replace("-", "")
}

/// 获取 project_id 的 workspace_path
async fn get_project_workspace(project_id: &str) -> Result<PathBuf> {
    let workspace_dir = PathBuf::from("./project_workspace");
    let project_dir = workspace_dir.join(project_id);
    Ok(project_dir)
}

/// 创建项目工作目录
async fn create_project_workspace(project_id: &str) -> Result<PathBuf> {
    let workspace_dir = PathBuf::from("./project_workspace");

    // 创建 project_workspace 目录（如果不存在）
    tokio::fs::create_dir_all(&workspace_dir).await?;

    // 创建项目目录
    let project_dir = workspace_dir.join(project_id);
    tokio::fs::create_dir_all(&project_dir).await?;

    info!("📁 创建项目工作目录: {:?}", project_dir);
    Ok(project_dir)
}

/// 处理聊天请求 - 使用 ACP 协议集成
///
/// 发送聊天消息并获取 AI 响应。支持多媒体附件，包括文本、图像、音频和文档。
/// 如果未提供 project_id，系统会自动生成新的项目 ID 并创建相应的工作目录。
/// 如果未提供 session_id，系统会创建新的会话。
#[utoipa::path(
    post,
    path = "/chat",
    request_body(
        content = ChatRequest,
        description = "聊天请求，包含用户输入的 prompt 和可选的多媒体附件",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功处理聊天请求，返回项目 ID 和会话 ID",
            body = HttpResult<ChatResponse>,
            example = json!({
                "success": true,
                "data": {
                    "project_id": "test_project",
                    "session_id": "session456",
                    "error": null
                },
                "error": null
            })
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "VALIDATION001",
                    "message": "Invalid request parameters"
                }
            })
        ),
        (
            status = 409,
            description = "Agent正在执行任务，禁止并发请求",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "code": "1010",
                "message": "Agent正在执行任务，请等待当前任务完成后再发送新请求",
                "tid": null
            })
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "INTERNAL001",
                    "message": "Internal server error"
                }
            })
        )
    ),
    tag = "chat",
    operation_id = "handle_chat",
    summary = "发送聊天消息",
    description = "通过 ACP 协议发送聊天消息给 AI 代理，支持文本和多媒体内容"
)]
#[axum::debug_handler]
#[instrument(skip(_state))]
pub async fn handle_chat(
    State(_state): State<Arc<AppState>>,
    Json(request): Json<ChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    info!(
        "🚀 [FORWARD] 开始转发 /chat 请求: project_id={:?}, session_id={:?}",
        request.project_id, request.session_id
    );

    // 直接转发原始请求到容器内的 agent_runner
    let result = forward_request_to_container(&request).await;

    info!("✅ [FORWARD] /chat 请求转发完成");
    result
}

/// 清理指定项目的 session 数据
async fn cleanup_sessions_for_project(project_id: &str, session_id: &Option<String>) {
    if let Some(session_id) = session_id {
        // 移除指定的 session
        if SESSION_CACHE.remove(session_id).is_some() {
            info!("🗑️ [chat] 移除旧session: session_id={}, project_id={}", session_id, project_id);
        }
    } else {
        // 移除该项目的所有 session
        let mut cleared_sessions = 0;
        for session_entry in SESSION_CACHE.iter() {
            let current_session_id = session_entry.key();
            let session_data = session_entry.value();

            // 检查这个 session 是否属于该项目
            if PROJECT_SESSION_MAP.get(project_id)
                .map(|sid| sid.as_str() == current_session_id.as_str())
                .unwrap_or(false) {
                if SESSION_CACHE.remove(current_session_id).is_some() {
                    cleared_sessions += 1;
                }
            }
        }

        if cleared_sessions > 0 {
            info!("🗑️ [chat] 移除了项目的所有旧session: project_id={}, cleared_count={}", project_id, cleared_sessions);
        }
    }
}


/// 检查或创建容器，并返回容器服务 URL
async fn ensure_container_exists(project_id: &str, request: &ChatRequest) -> Result<(String, String)> {
    // 检查容器是否已存在
    if !PROJECT_AND_AGENT_INFO_MAP.contains_key(project_id) {
        info!("🏗️ [FORWARD] 容器不存在，创建新容器: project_id={}", project_id);

        // 构建基本的 ChatPrompt（只包含必要信息用于创建容器）
        let chat_prompt = shared_types::ChatPromptBuilder::default()
            .project_id(project_id.to_string())
            .project_path(get_project_workspace(project_id).await?)
            .session_id(request.session_id.clone())
            .prompt(request.prompt.clone())
            .attachments(request.attachments.clone())
            .data_source_attachments(request.data_source_attachments.clone())
            .agent_type(AgentType::from_model_provider(request.model_provider.as_ref()))
            .request_id(request.request_id.clone().unwrap_or_else(generate_request_id))
            .build()
            .map_err(|e| {
                error!("❌ [FORWARD] 构建 ChatPrompt 失败: {}", e);
                crate::AppError::internal_server_error(&format!("构建 ChatPrompt 失败: {}", e))
            })?;

        // 创建容器
        create_container_for_request(&chat_prompt, request.model_provider.clone()).await?;
    }

    // 获取容器服务 URL
    if let Some(agent_info) = PROJECT_AND_AGENT_INFO_MAP.get(project_id) {
        let docker_manager = std::sync::Arc::new(
            docker_manager::DockerManager::with_default_config().await
                .map_err(|e| {
                    error!("❌ [FORWARD] 创建 DockerManager 失败: {}", e);
                    crate::AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
                })?
        );

        // 获取容器 IP 地址
        let container_info = docker_manager.get_container_info(project_id);

        if let Some(container_info) = container_info {
            let server_url = docker_container_agent::get_container_ip(&docker_manager, &container_info.container_id, container_info.assigned_port).await
                .map_err(|e| {
                    error!("❌ [FORWARD] 获取容器 IP 失败: {}", e);
                    crate::AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
                })?;

            info!("✅ [FORWARD] 获取容器服务 URL: {}", server_url);
            Ok((server_url, project_id.to_string()))
        } else {
            Err(crate::AppError::internal_server_error("未找到容器信息").into())
        }
    } else {
        Err(crate::AppError::internal_server_error("容器创建失败").into())
    }
}

/// 为请求创建容器
async fn create_container_for_request(
    chat_prompt: &ChatPrompt,
    model_provider: Option<ModelProviderConfig>,
) -> Result<()> {
    let project_id = &chat_prompt.project_id;
    info!("🏗️ [FORWARD] 开始为请求创建容器: project_id={}", project_id);

    // 使用 docker_container_agent 创建容器
    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config().await
            .map_err(|e| {
                error!("❌ [FORWARD] 创建 DockerManager 失败: project_id={}, error={}", project_id, e);
                e
            })?
    );

    let connection_info = docker_container_agent::start_docker_container_agent_service(
        chat_prompt.clone(),
        model_provider.clone(),
        docker_manager,
    ).await.map_err(|e| {
        error!("❌ [FORWARD] 创建容器失败: project_id={}, error={}", project_id, e);
        e
    })?;

    info!("✅ [FORWARD] 容器创建成功: project_id={}, session_id={}",
          project_id, connection_info.session_id);

    // 创建项目Agent信息并存储到 MAP 中
    let project_and_agent_info = ProjectAndAgentInfo {
        project_id: project_id.clone(),
        session_id: connection_info.session_id.clone(),
        prompt_tx: connection_info.prompt_tx.clone(),
        cancel_tx: connection_info.cancel_tx.clone(),
        model_provider,
        request_id: chat_prompt.request_id.clone(),
        status: AgentStatus::Idle,
        last_activity: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
    };

    // 存储到全局 MAP
    PROJECT_AND_AGENT_INFO_MAP.insert(project_id.clone(), project_and_agent_info);

    // 建立 project_id -> session_id 映射
    let session_id_str = connection_info.session_id.to_string();
    let cleared_old = crate::service::session_cache::ensure_project_session(project_id, &session_id_str).await;
    if cleared_old > 0 {
        info!("🧹 Project session 映射更新，已清理旧消息: project_id={}, cleared_count={}",
              project_id, cleared_old);
    }

    info!("✅ [FORWARD] 容器创建完成并已注册: project_id={}", project_id);
    Ok(())
}


/// 转发原始 HTTP 请求到容器内的 agent_runner
async fn forward_request_to_container(request: &ChatRequest) -> Result<crate::HttpResult<ChatResponse>, crate::AppError> {
    let project_id = request.project_id.as_deref().unwrap_or_else(|| "default");
    let session_id = request.session_id.as_deref().unwrap_or_else(|| "default");

    info!("📤 [FORWARD] 转发原始请求到容器: project_id={}, session_id={}", project_id, session_id);

    // 检查或创建容器
    let (server_url, project_id_final) = ensure_container_exists(&project_id, request).await?;

    // 直接转发原始 JSON 请求到容器内的 agent_runner
    let client = Client::new();
    let chat_url = format!("{}/chat", server_url);

    let response = client
        .post(&chat_url)
        .json(request)
        .send()
        .await
        .map_err(|e| {
            error!("❌ [FORWARD] 转发请求失败: {}", e);
            crate::AppError::internal_server_error(&format!("转发请求到容器失败: {}", e))
        })?;

    if response.status().is_success() {
        // 直接返回容器内的响应
        let container_response: ChatResponse = response.json().await
            .map_err(|e| {
                error!("❌ [FORWARD] 解析容器响应失败: {}", e);
                crate::AppError::internal_server_error(&format!("解析容器响应失败: {}", e))
            })?;

        info!("✅ [FORWARD] 容器响应成功: session_id={}", container_response.session_id);
        Ok(crate::HttpResult::success(container_response))
    } else {
        let error_text = format!("容器返回错误状态: {}", response.status());
        error!("❌ [FORWARD] {}", error_text);
        Ok(crate::HttpResult::error(
            "CONTAINER_ERROR",
            &error_text,
        ))
    }
}

/// 构建 Prompt 请求（从 acp_agent.rs 移过来）
async fn build_prompt_to_acp_agent(
    prompt: ChatPrompt,
    session_id: agent_client_protocol::SessionId,
) -> Result<agent_client_protocol::PromptRequest> {
    use agent_client_protocol::{ContentBlock, TextContent};

    // 构建最终提示词（包含系统提示词、用户输入和数据源信息）
    let final_prompt = if prompt.data_source_attachments.is_empty() {
        PromptBuilder::new().build(&prompt.prompt)
    } else {
        PromptBuilder::new()
            .build_with_data_sources(&prompt.prompt, &prompt.data_source_attachments)
    };

    // 创建文本内容块
    let text_block = ContentBlock::Text(TextContent {
        text: final_prompt,
        annotations: None,
        meta: None,
    });

    // 创建内容块列表，以文本开始
    let mut content_blocks = vec![text_block];

    // 如果有附件，转换为内容块
    if !prompt.attachments.is_empty() {
        let attachment_blocks = crate::utils::ContentBuilder::attachments_to_content_blocks(
            &prompt.attachments,
            &prompt.project_path,
        )
        .await?;

        content_blocks.extend(attachment_blocks);
    }

    // 将 request_id 放入 meta 字段
    let meta = if let Some(request_id) = prompt.request_id {
        Some(serde_json::json!({
            "request_id": request_id
        }))
    } else {
        None
    };

    Ok(agent_client_protocol::PromptRequest {
        session_id,
        prompt: content_blocks,
        meta,
    })
}
