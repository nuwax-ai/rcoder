//! 聊天处理器

use anyhow::Result;
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use shared_types::{ChatPromptBuilder, ModelProviderConfig};
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, error, info, instrument};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::proxy_agent::*;
use crate::service::AGENT_REGISTRY;
use crate::{router::AppState, *};

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

/// 服务响应结构
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatResponse {
    /// 项目 ID
    #[schema(example = "test_project")]
    pub project_id: String,
    /// 会话 ID
    #[schema(example = "session456")]
    pub session_id: String,
    /// 可选的错误信息
    pub error: Option<String>,
    /// 请求ID，用于标识和追踪请求
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
    // RCoder 容器的项目工作目录固定在 /app/project_workspace
    // 对应 docker/config.yml 中的 projects_dir 配置
    let workspace_base = "/app/project_workspace";

    let workspace_dir = PathBuf::from(workspace_base);
    let project_dir = workspace_dir.join(project_id);
    Ok(project_dir)
}

/// 创建项目工作目录
async fn create_project_workspace(project_id: &str) -> Result<PathBuf> {
    // RCoder 容器的项目工作目录固定在 /app/project_workspace
    // 对应 docker/config.yml 中的 projects_dir 配置
    let workspace_base = "/app/project_workspace";

    let workspace_dir = PathBuf::from(workspace_base);

    // 创建基础目录（如果不存在）
    tokio::fs::create_dir_all(&workspace_dir).await?;

    // 创建项目目录
    let project_dir = workspace_dir.join(project_id);
    tokio::fs::create_dir_all(&project_dir).await?;

    info!(
        "📁 创建项目工作目录: {:?} (base: {})",
        project_dir, workspace_base
    );
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
#[instrument(skip(state, request))]
pub async fn handle_chat(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatRequest>,
) -> Result<crate::model::HttpResult<ChatResponse>, crate::model::AppError> {
    // 验证prompt不能为空
    if request.prompt.trim().is_empty() {
        return Err(crate::model::AppError::validation_error(
            "prompt 字段不能为空",
        ));
    }

    // agent_runner 运行在容器内部，固定使用 RCoder 服务类型
    let service_type = shared_types::ServiceType::RCoder;

    info!(
        "🚀 [DEBUG] handle_chat 开始处理请求: project_id={:?}, session_id={:?}, service_type={}, prompt={}",
        request.project_id, request.session_id, service_type, request.prompt
    );

    // 检查是否需要生成项目ID
    let project_id = match request.project_id.clone() {
        Some(id) => {
            debug!("📝 [DEBUG] 使用请求中的项目ID: {:?}", id);
            id
        }
        None => {
            let new_project_id = generate_project_id();
            debug!("🆕 [DEBUG] 生成新的项目ID: {}", new_project_id);

            // 创建项目工作目录
            create_project_workspace(&new_project_id).await?;
            new_project_id
        }
    };

    // 🚦 检查 Agent 状态，禁止并发请求（使用统一 Registry）
    if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&project_id)
        && agent_info.status == AgentStatus::Active
    {
        info!(
            "⚠️ [chat] Agent正在执行任务，拒绝并发请求: project_id={}, status={:?}",
            project_id, agent_info.status
        );
        return Ok(crate::model::HttpResult::error(
            shared_types::error_codes::ERR_AGENT_BUSY,
            "Agent正在执行任务，请等待当前任务完成后再发送新请求",
        ));
    }

    // 🗑️ 简单直接：直接移除所有相关session，确保全新开始
    // 新的设计：移除旧session → Agent必须重新获取sender → 前端只能获取新消息
    if let Some(ref session_id) = request.session_id {
        // 直接移除指定session
        if crate::service::SESSION_CACHE.remove(session_id).is_some() {
            info!(
                "🗑️ [chat] 移除旧session，确保全新开始: session_id={}, project_id={}",
                session_id, project_id
            );
        }
    } else {
        // 如果没有指定session_id，清空该项目的所有session
        let mut cleared_sessions = 0;
        for session_entry in state.sessions.iter() {
            let current_session_id = session_entry.key();
            let session_info = session_entry.value();

            if let Some(session_project_id) = &session_info.project_id
                && session_project_id == &project_id
                && crate::service::SESSION_CACHE
                    .remove(current_session_id)
                    .is_some()
            {
                cleared_sessions += 1;
            }
        }

        if cleared_sessions > 0 {
            info!(
                "🗑️ [chat] 移除了项目的所有旧session: project_id={}, cleared_count={}",
                project_id, cleared_sessions
            );
        }
    }

    // 获取项目工作目录
    let project_workspace = get_project_workspace(&project_id).await?;

    // 确定或生成 request_id
    let request_id = request
        .request_id
        .clone()
        .unwrap_or_else(generate_request_id);

    let chat_prompt = ChatPromptBuilder::default()
        .project_id(project_id.clone())
        .project_path(project_workspace)
        .session_id(request.session_id.clone())
        .prompt(request.prompt.clone())
        .attachments(request.attachments.clone())
        .data_source_attachments(request.data_source_attachments.clone())
        .service_type(service_type) // agent_runner 固定使用 RCoder
        .request_id(request_id.clone())
        .build()
        .map_err(|e| anyhow::anyhow!(e))?;

    // 转换为 PromptMessage（Agent 抽象层）
    let prompt_message = agent_abstraction::PromptMessage::from(chat_prompt);

    let (local_task_request, chat_prompt_rx) =
        LocalSetAgentRequest::new(prompt_message, request.model_provider.clone());
    state.local_task_sender.send(local_task_request)?;

    let result = match chat_prompt_rx.await {
        Ok(chat_prompt_response) => {
            // 检查响应中是否有错误
            if let Some(error_msg) = chat_prompt_response.error {
                error!(
                    "❌ Agent 处理失败: project_id={}, session_id={}, error={}",
                    chat_prompt_response.project_id, chat_prompt_response.session_id, error_msg
                );
                // 🎯 使用响应中的 code 字段，而不是硬编码错误码
                crate::model::HttpResult::error(&chat_prompt_response.code, &error_msg)
            } else {
                info!(
                    "✅ 收到 agent 执行结果: project_id={}, session_id={}",
                    chat_prompt_response.project_id, chat_prompt_response.session_id,
                );
                crate::model::HttpResult::success(ChatResponse {
                    project_id: chat_prompt_response.project_id,
                    session_id: chat_prompt_response.session_id,
                    error: None,
                    request_id: request.request_id.clone(),
                })
            }
        }
        Err(e) => {
            error!("❌ 收到 agent 执行结果失败: {}", e);
            crate::model::HttpResult::error(
                "LOCAL001",
                &format!("Local task sender send error: {}", e),
            )
        }
    };
    Ok(result)
}
