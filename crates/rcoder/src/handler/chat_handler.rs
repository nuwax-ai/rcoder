//! 聊天处理器
//!
//! 将原始 HTTP 请求直接转发到容器内的 agent_runner 服务

use anyhow::Result;
use axum::{Json, extract::State};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use shared_types::ModelProviderConfig;
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, error, info, instrument};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::proxy_agent::{PROJECT_AND_AGENT_INFO_MAP, docker_container_agent};
use crate::service::session_cache::{PROJECT_SESSION_MAP, SESSION_CACHE};
use crate::utils::prompt_builder::PromptBuilder;
use crate::{router::AppState, *};
use docker_manager::ContainerBasicInfo;
use shared_types::{AgentType, ChatPromptBuilder};

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

/// 处理聊天请求 - 转发到容器化 agent_runner 服务
///
/// 1. 根据 project_id 检查或动态创建对应的容器
/// 2. 将原始聊天请求直接转发到容器内的 agent_runner 服务
/// 3. 获取并返回 agent_runner 的处理结果
///
/// 注意：所有参数处理（如 project_id、session_id 生成）都由 agent_runner 处理
/// RCoder 只负责容器管理和请求转发
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
            description = "成功处理聊天请求",
            body = HttpResult<ChatResponse>,
            example = json!({
                "success": true,
                "data": {
                    "project_id": "test_project",
                    "session_id": "session456",
                    "error": null,
                    "request_id": "req_123456789"
                },
                "error": null
            })
        ),
        (
            status = 500,
            description = "服务器内部错误或容器服务异常",
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
    summary = "转发聊天消息到容器化 AI 服务",
    description = "根据 project_id 动态管理容器，将原始聊天请求直接转发到容器内的 agent_runner 服务进行处理"
)]
#[axum::debug_handler]
#[instrument(skip(_state), fields(project_id = ?request.project_id, session_id = ?request.session_id))]
pub async fn handle_chat(
    State(_state): State<Arc<AppState>>,
    Json(request): Json<ChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    let project_id = request.project_id.as_deref().unwrap_or("default");

    info!(
        "🚀 [CHAT] 开始处理聊天请求: project_id={}, session_id={:?}, prompt_length={}, attachments_count={}",
        project_id,
        request.session_id,
        request.prompt.len(),
        request.attachments.len()
    );

    // 直接转发原始请求到容器内的 agent_runner 服务
    // agent_runner 会处理所有业务逻辑，包括：
    // - project_id 和 session_id 的生成
    // - 会话管理
    // - AI 处理

    // 第一步：获取或创建容器
    let container_info =
        crate::service::container_manager::ContainerManager::get_or_create_container(
            project_id, &request,
        )
        .await?;

    // 第二步：转发请求到容器服务
    let result = forward_request_to_container_service(&request, &container_info).await;

    match &result {
        Ok(_) => {
            info!("✅ [CHAT] 聊天请求处理成功: project_id={}", project_id);
        }
        Err(e) => {
            error!(
                "❌ [CHAT] 聊天请求处理失败: project_id={}, error={}",
                project_id, e
            );
        }
    }

    result
}

/// 清理指定项目的 session 数据
async fn cleanup_sessions_for_project(project_id: &str, session_id: &Option<String>) {
    if let Some(session_id) = session_id {
        // 移除指定的 session
        if SESSION_CACHE.remove(session_id).is_some() {
            info!(
                "🗑️ [chat] 移除旧session: session_id={}, project_id={}",
                session_id, project_id
            );
        }
    } else {
        // 移除该项目的所有 session
        let mut cleared_sessions = 0;
        for session_entry in SESSION_CACHE.iter() {
            let current_session_id = session_entry.key();
            let session_data = session_entry.value();

            // 检查这个 session 是否属于该项目
            if PROJECT_SESSION_MAP
                .get(project_id)
                .map(|sid| sid.as_str() == current_session_id.as_str())
                .unwrap_or(false)
            {
                if SESSION_CACHE.remove(current_session_id).is_some() {
                    cleared_sessions += 1;
                }
            }
        }

        if cleared_sessions > 0 {
            info!(
                "🗑️ [chat] 移除了项目的所有旧session: project_id={}, cleared_count={}",
                project_id, cleared_sessions
            );
        }
    }
}

/// 根据 project_id 检查对应容器是否存在，不存在就动态创建容器
async fn ensure_container_exists(
    project_id: &str,
    request: &ChatRequest,
) -> Result<ContainerBasicInfo> {
    info!("🔍 [FORWARD] 检查容器是否存在: project_id={}", project_id);

    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config()
            .await
            .map_err(|e| {
                error!("❌ [FORWARD] 创建 DockerManager 失败: {}", e);
                crate::AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?,
    );

    // 首先检查容器是否已存在
    if let Some(existing_container_info) = docker_manager.get_container_info(project_id) {
        info!(
            "✅ [FORWARD] 容器已存在: project_id={}, container_id={}",
            project_id, existing_container_info.container_id
        );

        // 获取容器IP地址
        let server_url = docker_container_agent::get_container_ip(
            &docker_manager,
            &existing_container_info.container_id,
            existing_container_info.assigned_port,
        )
        .await
        .map_err(|e| {
            error!("❌ [FORWARD] 获取容器 IP 失败: {}", e);
            crate::AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
        })?;

        // 从URL中提取IP地址
        let ip_address = extract_ip_from_url(&server_url)?;

        let container_info = ContainerBasicInfo {
            container_id: existing_container_info.container_id.clone(),
            container_name: existing_container_info.container_name.clone(),
            container_ip: ip_address,
            internal_port: existing_container_info.internal_port,
            external_port: existing_container_info.assigned_port,
            project_id: project_id.to_string(),
            session_id: existing_container_info.session_id.clone(),
            status: existing_container_info.status.to_string(),
            created_at: existing_container_info.created_at,
            service_url: server_url,
        };

        info!(
            "✅ [FORWARD] 返回已存在容器信息: IP={}, external_port={}",
            container_info.container_ip, container_info.external_port
        );

        return Ok(container_info);
    }

    // 容器不存在，需要创建新容器
    info!(
        "🏗️ [FORWARD] 容器不存在，创建新容器: project_id={}",
        project_id
    );
    create_container_for_chat_request(project_id, request, &docker_manager).await
}

/// 从URL中提取IP地址
fn extract_ip_from_url(url: &str) -> Result<String, crate::AppError> {
    let url_obj = url::Url::parse(url).map_err(|e| {
        error!("❌ [FORWARD] 解析URL失败: url={}, error={}", url, e);
        crate::AppError::internal_server_error(&format!("解析URL失败: {}", e))
    })?;

    let host = url_obj.host_str().ok_or_else(|| {
        error!("❌ [FORWARD] URL中找不到主机地址: {}", url);
        crate::AppError::internal_server_error("URL中找不到主机地址")
    })?;

    Ok(host.to_string())
}

/// 为聊天请求创建容器
async fn create_container_for_chat_request(
    project_id: &str,
    request: &ChatRequest,
    docker_manager: &std::sync::Arc<docker_manager::DockerManager>,
) -> Result<ContainerBasicInfo> {
    info!(
        "🏗️ [FORWARD] 开始为聊天请求创建容器: project_id={}",
        project_id
    );

    // 确保项目工作目录存在
    let project_workspace = get_project_workspace(project_id).await?;
    create_project_workspace(project_id).await.map_err(|e| {
        error!(
            "❌ [FORWARD] 创建项目工作目录失败: project_id={}, error={}",
            project_id, e
        );
        crate::AppError::internal_server_error(&format!("创建项目工作目录失败: {}", e))
    })?;

    // 生成会话ID和请求ID
    let session_id = request.session_id.clone().unwrap_or_else(|| {
        format!(
            "session_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        )
    });
    let request_id = request
        .request_id
        .clone()
        .unwrap_or_else(generate_request_id);

    // 启动容器（主要目的是创建容器和通信通道）
    let (container_info_docker, server_url) =
        docker_container_agent::start_docker_container_agent_service(
            project_id.to_string(),
            project_workspace.to_string_lossy().to_string(),
            docker_manager.clone(),
        )
        .await
        .map_err(|e| {
            error!(
                "❌ [FORWARD] 创建容器失败: project_id={}, error={}",
                project_id, e
            );
            crate::AppError::internal_server_error(&format!("创建容器失败: {}", e))
        })?;

    info!(
        "✅ [FORWARD] 容器创建成功: project_id={}, session_id={}",
        project_id, session_id
    );

    // 获取容器详细信息
    let container_info_docker = docker_manager
        .get_container_info(project_id)
        .ok_or_else(|| {
            error!(
                "❌ [FORWARD] 新创建的容器信息获取失败: project_id={}",
                project_id
            );
            crate::AppError::internal_server_error("新创建的容器信息获取失败")
        })?;

    // 获取容器IP地址
    let server_url = docker_container_agent::get_container_ip(
        docker_manager,
        &container_info_docker.container_id,
        container_info_docker.assigned_port,
    )
    .await
    .map_err(|e| {
        error!("❌ [FORWARD] 获取新容器 IP 失败: {}", e);
        crate::AppError::internal_server_error(&format!("获取新容器 IP 失败: {}", e))
    })?;

    let ip_address = extract_ip_from_url(&server_url)?;

    // 创建容器基本信息结构
    let container_info = ContainerBasicInfo {
        container_id: container_info_docker.container_id.clone(),
        container_name: container_info_docker.container_name.clone(),
        container_ip: ip_address,
        internal_port: container_info_docker.internal_port,
        external_port: container_info_docker.assigned_port,
        project_id: project_id.to_string(),
        session_id: session_id.clone(),
        status: container_info_docker.status.to_string(),
        created_at: container_info_docker.created_at,
        service_url: server_url.clone(),
    };

    info!(
        "✅ [FORWARD] 容器创建完成: project_id={}, container_id={}, IP={}, workspace={:?}",
        project_id, container_info.container_id, container_info.container_ip, project_workspace
    );

    Ok(container_info)
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
        docker_manager::DockerManager::with_default_config()
            .await
            .map_err(|e| {
                error!(
                    "❌ [FORWARD] 创建 DockerManager 失败: project_id={}, error={}",
                    project_id, e
                );
                e
            })?,
    );

    // 创建项目工作目录
    let project_workspace = get_project_workspace(project_id).await?;
    create_project_workspace(project_id).await.map_err(|e| {
        error!(
            "❌ [FORWARD] 创建项目工作目录失败: project_id={}, error={}",
            project_id, e
        );
        e
    })?;

    let (_container_info, _server_url) =
        docker_container_agent::start_docker_container_agent_service(
            project_id.to_string(),
            project_workspace.to_string_lossy().to_string(),
            docker_manager,
        )
        .await
        .map_err(|e| {
            error!(
                "❌ [FORWARD] 创建容器失败: project_id={}, error={}",
                project_id, e
            );
            e
        })?;

    info!("✅ [FORWARD] 容器创建成功: project_id={}", project_id);
    Ok(())
}

/// 转发请求到容器内的 agent_runner 服务
///
/// 将原始聊天请求直接转发到指定的容器服务，获取并返回处理结果
async fn forward_request_to_container_service(
    request: &ChatRequest,
    container_info: &ContainerBasicInfo,
) -> Result<crate::HttpResult<ChatResponse>, crate::AppError> {
    let project_id = request.project_id.as_deref().unwrap_or("default");

    info!(
        "📤 [FORWARD] 转发请求到容器: project_id={}, container_id={}, service_url={}",
        project_id, container_info.container_id, container_info.service_url
    );

    // 直接转发原始 JSON 请求到容器内的 agent_runner
    // 不做任何参数修改，让 agent_runner 处理所有业务逻辑
    let client = Client::new();
    let chat_url = format!("{}/chat", container_info.service_url);

    debug!(
        "📡 [FORWARD] 发送HTTP请求到: {}, body: prompt_length={}, attachments_count={}",
        chat_url,
        request.prompt.len(),
        request.attachments.len()
    );

    let response = client
        .post(&chat_url)
        .json(request) // 直接转发原始请求
        .send()
        .await
        .map_err(|e| {
            error!("❌ [FORWARD] HTTP请求失败: {}", e);
            crate::AppError::internal_server_error(&format!("转发请求到容器失败: {}", e))
        })?;

    let status = response.status();
    debug!("📥 [FORWARD] 容器响应状态: {}", status);

    if status.is_success() {
        // 直接返回容器内的响应，不做任何修改
        let container_response: ChatResponse = response.json().await.map_err(|e| {
            error!("❌ [FORWARD] 解析容器响应失败: {}", e);
            crate::AppError::internal_server_error(&format!("解析容器响应失败: {}", e))
        })?;

        info!(
            "✅ [FORWARD] 容器响应成功: project_id={}, session_id={}",
            container_response.project_id, container_response.session_id
        );
        Ok(crate::HttpResult::success(container_response))
    } else {
        let error_text = format!("容器返回错误状态: {}", status);
        let response_body = response.text().await.unwrap_or_default();
        error!("❌ [FORWARD] {}, 响应内容: {}", error_text, response_body);
        Ok(crate::HttpResult::error("CONTAINER_ERROR", &error_text))
    }
}

/// 转发原始请求到容器内的 agent_runner 服务（组合函数）
///
/// 1. 获取或创建容器
/// 2. 转发请求到容器服务
async fn forward_request_to_container(
    request: &ChatRequest,
) -> Result<crate::HttpResult<ChatResponse>, crate::AppError> {
    let project_id = request.project_id.as_deref().unwrap_or("default");

    info!(
        "📤 [FORWARD] 开始转发请求: project_id={}, session_id={:?}",
        project_id, request.session_id
    );

    // 第一步：获取或创建容器
    let container_info =
        crate::service::container_manager::ContainerManager::get_or_create_container(
            project_id, request,
        )
        .await?;

    // 第二步：转发请求到容器服务
    forward_request_to_container_service(request, &container_info).await
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
