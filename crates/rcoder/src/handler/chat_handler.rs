//! 聊天处理器

use anyhow::Result;
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use shared_types::ModelProviderConfig;
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, error, info, instrument};
use uuid::Uuid;
use utoipa::ToSchema;

use crate::proxy_agent::*;
use crate::{model::*, router::AppState};

/// 用户请求结构 - 支持多媒体内容
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct ChatRequest {
    /// 用户输入的 prompt
    #[schema(example = "帮我写一个 Rust 的 Hello World 程序")]
    pub prompt: String,
    /// 用户 ID
    #[schema(example = "user123")]
    pub user_id: String,
    /// 可选的项目 ID
    #[schema(example = "test_project")]
    pub project_id: Option<String>,
    /// 可选的会话 ID，如果不提供则创建新会话
    #[schema(example = "session456")]
    pub session_id: Option<String>,
    /// 可选的附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// 模型配置
    #[schema(example = "openai")]
    pub model_provider: Option<ModelProviderConfig>,
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
}

/// 生成不带中划线的随机项目ID
fn generate_project_id() -> String {
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
#[instrument(skip(state))]
pub async fn handle_chat(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatRequest>,
) -> Result<crate::model::HttpResult<ChatResponse>, crate::model::AppError> {
    info!(
        "🚀 [DEBUG] handle_chat 开始处理请求: user_id={}, project_id={:?}, session_id={:?}, prompt={}",
        request.user_id, request.project_id, request.session_id, request.prompt
    );

    // 检查是否需要生成项目ID
    let project_id = if request.project_id.is_some() {
        debug!("📝 [DEBUG] 使用请求中的项目ID: {:?}", request.project_id);
        request.project_id.clone().unwrap()
    } else {
        let new_project_id = generate_project_id();
        debug!("🆕 [DEBUG] 生成新的项目ID: {}", new_project_id);

        // 创建项目工作目录
        create_project_workspace(&new_project_id).await?;
        new_project_id
    };

    // 获取项目工作目录
    let project_workspace = get_project_workspace(&project_id).await?;


    // 根据模型提供商配置自动选择 agent 类型
    let agent_type = AgentType::from_model_provider(request.model_provider.as_ref());

    let chat_prompt = ChatPromptBuilder::default()
        .project_id(project_id.clone())
        .project_path(project_workspace)
        .session_id(request.session_id.clone())
        .prompt(request.prompt.clone())
        .attachments(request.attachments.clone())
        .agent_type(agent_type)
        .build()
        .map_err(|e| anyhow::anyhow!(e))?;

    let (local_task_request, chat_prompt_rx) = LocalSetAgentRequest::new(chat_prompt, request.model_provider.clone());
    state.local_task_sender.send(local_task_request)?;

    let result = match chat_prompt_rx.await {
        Ok(chat_prompt_response) => {
            info!(
                "✅ 收到 agent 执行结果: project_id={}, session_id={}",
                chat_prompt_response.project_id, chat_prompt_response.session_id,
            );
            crate::model::HttpResult::success(ChatResponse {
                project_id: chat_prompt_response.project_id,
                session_id: chat_prompt_response.session_id,
                error: None,
            })
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
