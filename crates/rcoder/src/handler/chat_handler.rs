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
    Json(mut request): Json<ChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    let project_id = match &request.project_id {
        Some(id) => id.clone(),
        None => {
            let project_id = crate::service::container_manager::generate_project_id();
            request.project_id = Some(project_id.clone());  // 设置 project_id
            project_id
        }
    };

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
        crate::service::container_manager::ContainerManager::get_or_create_container(&project_id)
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

/// 转发请求到容器内的 agent_runner 服务
///
/// 将原始聊天请求直接转发到指定的容器服务，获取并返回处理结果
async fn forward_request_to_container_service(
    request: &ChatRequest,
    container_info: &ContainerBasicInfo,
) -> Result<crate::HttpResult<ChatResponse>, crate::AppError> {
    let project_id = if let Some(id) = &request.project_id {
        id.clone()
    } else {
        error!("[FORWARD]会话 project_id 不能为空");
        return Err(crate::AppError::internal_server_error(
            "project_id 不能为空",
        ));
    };

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
        // 解析容器的 HttpResult<ChatResponse> 响应
        let container_http_result: shared_types::HttpResult<ChatResponse> =
            response.json().await.map_err(|e| {
                error!("❌ [FORWARD] 解析容器响应失败: {}", e);
                crate::AppError::internal_server_error(&format!("解析容器响应失败: {}", e))
            })?;

        // 提取 data 字段中的 ChatResponse
        let container_response = container_http_result.data.ok_or_else(|| {
            error!("❌ [FORWARD] 容器响应缺少 data 字段");
            crate::AppError::internal_server_error("容器响应缺少 data 字段")
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
