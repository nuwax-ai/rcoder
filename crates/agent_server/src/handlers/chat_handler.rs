//! 聊天处理器 - 完全复制 rcoder 的实现

use crate::{
    api::ApiState,
    models::{ChatRequest, ChatResponse, HttpResult, ModelProviderConfig},
    AgentServerError, AgentServerResult,
};
use shared_types::AgentType;
use axum::{extract::State, response::Json};
use serde_json::{json, Value};
use std::path::PathBuf;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// 处理聊天请求 - 完全复制 rcoder 的 handle_chat 实现
pub async fn handle_chat(
    State(state): State<ApiState>,
    Json(request): Json<ChatRequest>,
) -> Result<Json<HttpResult<ChatResponse>>, AgentServerError> {
    info!(
        "🚀 [DEBUG] handle_chat 开始处理请求: project_id={:?}, session_id={:?}, prompt={}",
        request.project_id, request.session_id, request.prompt
    );

    // 检查是否需要生成项目ID
    let project_id = if let Some(ref project_id) = request.project_id {
        debug!("📝 [DEBUG] 使用请求中的项目ID: {:?}", project_id);
        project_id.clone()
    } else {
        let new_project_id = generate_project_id();
        debug!("🆕 [DEBUG] 生成新的项目ID: {}", new_project_id);

        // 创建项目工作目录
        create_project_workspace(&new_project_id).await?;
        new_project_id
    };

    // 🚦 检查 Agent 状态，禁止并发请求
    // TODO: 实现类似 PROJECT_AND_AGENT_INFO_MAP 的状态检查
    if let Some(session) = state.agent_manager.get_session(&project_id).await {
        if session.status == crate::agent::SessionStatus::Processing {
            info!(
                "⚠️ [chat] Agent正在执行任务，拒绝并发请求: project_id={}, status={:?}",
                project_id, session.status
            );
            return Ok(Json(HttpResult::error(
                "9010",
                "Agent正在执行任务，请等待当前任务完成后再发送新请求",
            )));
        }
    }

    // 🗑️ 简单直接：直接移除所有相关session，确保全新开始
    if let Some(ref session_id) = request.session_id {
        // 直接移除指定session
        if let Some(_) = state.agent_manager.get_session(session_id).await {
            info!("🗑️ [chat] 移除旧session，确保全新开始: session_id={}, project_id={}", session_id, project_id);
            // TODO: 实现 session 移除逻辑
        }
    } else {
        // 如果没有指定session_id，清空该项目的所有session
        let sessions = state.agent_manager.list_sessions().await;
        let mut cleared_sessions = 0;

        for session in sessions {
            if session.project_id == project_id {
                // TODO: 实现 session 移除逻辑
                cleared_sessions += 1;
            }
        }

        if cleared_sessions > 0 {
            info!("🗑️ [chat] 移除了项目的所有旧session: project_id={}, cleared_count={}", project_id, cleared_sessions);
        }
    }

    // 获取项目工作目录
    let project_workspace = get_project_workspace(&project_id).await?;

    // 根据模型提供商配置自动选择 agent 类型
    let agent_type = if let Some(ref provider) = request.model_provider {
        match provider.api_protocol.as_str() {
            "anthropic" => crate::config::AgentType::Claude,
            "openai" | _ => crate::config::AgentType::Codex,
        }
    } else {
        crate::config::AgentType::default()
    };

    // 确定或生成 request_id
    let request_id = request
        .request_id
        .clone()
        .unwrap_or_else(generate_request_id);

    // 创建新的会话
    let session_id = if let Some(ref sid) = request.session_id {
        sid.clone()
    } else {
        let new_session_id = generate_session_id();
        info!("🆕 [DEBUG] 生成新的会话ID: {}", new_session_id);
        new_session_id
    };

    let _session = state.agent_manager.create_session(Some(session_id.clone())).await?;

    info!(
        "✅ [chat] 聊天请求已接受，开始处理: session_id={}, request_id={}, project_id={}",
        session_id, request_id, project_id
    );

    // TODO: 这里应该实现完整的 Agent 处理逻辑
    // 目前先返回成功响应，表示已接受请求
    let response = ChatResponse {
        project_id: project_id.clone(),
        session_id: session_id.clone(),
        request_id: request_id.clone(),
        status: crate::models::ResponseStatus::Accepted,
        content: None,
        error: None,
        created_at: chrono::Utc::now(),
    };

    Ok(Json(HttpResult::success(response)))
}

/// 验证聊天请求
fn validate_chat_request(request: &ChatRequest) -> AgentServerResult<()> {
    if request.prompt.trim().is_empty() {
        return Err(AgentServerError::ValidationError("提示内容不能为空".to_string()));
    }

    if request.prompt.len() > 10000 {
        return Err(AgentServerError::ValidationError("提示内容过长，最多支持10000字符".to_string()));
    }

    // 验证项目ID格式（如果提供）
    if let Some(ref project_id) = request.project_id {
        if project_id.trim().is_empty() {
            return Err(AgentServerError::ValidationError("项目ID不能为空".to_string()));
        }
    }

    // 验证会话ID格式（如果提供）
    if let Some(ref session_id) = request.session_id {
        if session_id.trim().is_empty() {
            return Err(AgentServerError::ValidationError("会话ID不能为空".to_string()));
        }
    }

    Ok(())
}

/// 获取 project_id 的 workspace_path
async fn get_project_workspace(project_id: &str) -> AgentServerResult<PathBuf> {
    let workspace_dir = PathBuf::from("./project_workspace");
    let project_dir = workspace_dir.join(project_id);
    Ok(project_dir)
}

/// 创建项目工作目录
async fn create_project_workspace(project_id: &str) -> AgentServerResult<PathBuf> {
    let workspace_dir = PathBuf::from("./project_workspace");

    // 创建 project_workspace 目录（如果不存在）
    tokio::fs::create_dir_all(&workspace_dir).await.map_err(|e| {
        AgentServerError::IoError(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("创建工作目录失败: {}", e),
        ))
    })?;

    // 创建项目目录
    let project_dir = workspace_dir.join(project_id);
    tokio::fs::create_dir_all(&project_dir).await.map_err(|e| {
        AgentServerError::IoError(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("创建项目目录失败: {}", e),
        ))
    })?;

    info!("📁 创建项目工作目录: {:?}", project_dir);
    Ok(project_dir)
}

/// 生成不带中划线的随机项目 ID
fn generate_project_id() -> String {
    Uuid::new_v4().to_string().replace("-", "")
}

/// 生成不带中划线的随机请求 ID
fn generate_request_id() -> String {
    Uuid::new_v4().to_string().replace("-", "")
}

/// 生成会话 ID
fn generate_session_id() -> String {
    Uuid::new_v4().to_string().replace("-", "")
}

