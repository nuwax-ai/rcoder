//! 聊天处理器

use anyhow::Result;
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, error, info, instrument};
use uuid::Uuid;

use crate::proxy_agent::*;
use crate::{model::*, router::AppState};

/// 用户请求结构 - 支持多媒体内容
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ChatRequest {
    /// 用户输入的 prompt
    pub prompt: String,
    /// 用户 ID
    pub user_id: String,
    /// 可选的项目 ID
    pub project_id: Option<String>,
    /// 可选的会话 ID，如果不提供则创建新会话
    pub session_id: Option<String>,
}

/// 服务响应结构
#[derive(Debug, Serialize)]
pub struct ChatResponse {
    /// 项目 ID
    pub project_id: String,
    /// 会话 ID
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
#[axum::debug_handler]
#[instrument(skip(state))]
pub async fn handle_chat(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChatRequest>,
) -> Result<Json<crate::model::HttpResult<ChatResponse>>, crate::model::AppError> {
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

    let chat_prompt = ChatPrompt {
        project_id: project_id.clone(),
        project_path: project_workspace,
        session_id: request.session_id.clone(),
        prompt: request.prompt.clone(),
    };

    let (local_task_request, chat_prompt_rx) = LocalSetAgentRequest::new(chat_prompt);
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
    Ok(Json(result))
}
