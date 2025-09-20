use crate::{SharedState, ProgressEvent, ProgressEventType, broadcast_progress_event, get_trace_id, create_new_session, update_session_activity, execute_ai_command, ChatResponse, HttpResult};
use acp_adapter::mention::{ResourceUri, ResourceUriBuilder};
use acp_adapter::permission::{PermissionManager, PermissionEvent};
use acp_adapter::capability::{AgentConnection, PermissionCapability};
use agent_client_protocol::{PromptRequest, ContentBlock, TextContent, EmbeddedResourceResource, TextResourceContents};
use axum::{
    extract::{State, Multipart},
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn, error, debug};
use uuid::Uuid;
use std::sync::Arc;

/// 多媒体聊天请求结构 - 用于处理文件上传
#[derive(Debug)]
pub struct MultipartChatRequest {
    /// 用户输入的 prompt
    pub prompt: String,
    /// 用户 ID
    pub user_id: String,
    /// 可选的项目 ID
    pub project_id: Option<String>,
    /// 可选的会话 ID
    pub session_id: Option<String>,
    /// 上传的文件列表
    pub files: Vec<UploadedFile>,
    /// 代码片段列表
    pub code_snippets: Vec<CodeSnippet>,
    /// 选中的代码段引用
    pub code_references: Vec<ResourceUri>,
}

/// 上传的文件信息
#[derive(Debug)]
pub struct UploadedFile {
    /// 原文件名
    pub filename: String,
    /// MIME 类型
    pub content_type: String,
    /// 文件内容
    pub content: Vec<u8>,
    /// 文件大小
    pub size: usize,
    /// 生成的资源URI
    pub resource_uri: ResourceUri,
}

/// 代码片段
#[derive(Debug, Serialize, Deserialize)]
pub struct CodeSnippet {
    /// 代码内容
    pub content: String,
    /// 编程语言
    pub language: Option<String>,
    /// 文件路径（如果来自文件）
    pub file_path: Option<String>,
    /// 行号范围
    pub line_range: Option<(u32, u32)>,
    /// 描述或标题
    pub description: Option<String>,
}

/// 处理包含文件上传的聊天请求
pub async fn handle_multipart_chat(
    State(state): State<SharedState>,
    mut multipart: Multipart,
) -> HttpResult<ChatResponse> {
    let trace_id = get_trace_id();
    
    info!("收到多媒体聊天请求, trace_id={:?}", trace_id);

    // 解析 multipart 数据
    let mut request = match parse_multipart_request(&mut multipart, &state).await {
        Ok(req) => req,
        Err(e) => {
            error!("解析多媒体请求失败: {}", e);
            return HttpResult::error(
                "MULTIPART001",
                &format!("解析多媒体请求失败: {}", e),
                trace_id,
            );
        }
    };

    info!(
        "解析的多媒体请求: user_id={}, project_id={:?}, session_id={:?}, files_count={}, snippets_count={}, references_count={}",
        request.user_id, request.project_id, request.session_id, 
        request.files.len(), request.code_snippets.len(), request.code_references.len()
    );

    // 如果没有提供 project_id，则生成一个
    if request.project_id.is_none() {
        let new_project_id = Uuid::now_v7().to_string();
        info!("Generated new project_id: {}", new_project_id);
        request.project_id = Some(new_project_id);
    }

    // 创建项目目录
    if let Some(ref project_id) = request.project_id {
        let project_path = state.config.projects_dir.join(project_id);
        if !project_path.exists() {
            if let Err(e) = tokio::fs::create_dir_all(&project_path).await {
                error!("Failed to create project directory {:?}: {}", project_path, e);
                return HttpResult::error(
                    "DIR001",
                    &format!("Failed to create project directory: {}", e),
                    trace_id,
                );
            }
            info!("Created project directory: {:?}", project_path);
        }
    }

    // 获取或创建会话
    let session_id = match &request.session_id {
        Some(id) => {
            if state.sessions.contains_key(id) {
                id.clone()
            } else {
                warn!("Session {} not found, creating new session", id);
                create_new_session_for_multipart(&state, &request).await
            }
        }
        None => create_new_session_for_multipart(&state, &request).await,
    };

    // 处理上传的文件和代码片段，构建 ACP 内容
    let enhanced_prompt = match build_enhanced_prompt(&request).await {
        Ok(prompt) => prompt,
        Err(e) => {
            error!("构建增强prompt失败: {}", e);
            return HttpResult::error(
                "PROMPT001",
                &format!("构建增强prompt失败: {}", e),
                trace_id,
            );
        }
    };

    // 获取会话信息以确定使用的代理类型
    let agent_type = {
        state.sessions.get(&session_id)
            .map(|s| s.agent_type.clone())
            .unwrap_or(state.config.default_agent.clone())
    };

    // 构建传统的ChatRequest结构用于执行
    let chat_request = crate::ChatRequest {
        prompt: enhanced_prompt,
        user_id: request.user_id.clone(),
        project_id: request.project_id.clone(),
        session_id: Some(session_id.clone()),
    };

    // 调用 AI 代理处理请求
    match execute_ai_command(&agent_type, &chat_request, &state.config, &state, &session_id).await {
        Ok(response) => {
            // 更新会话活动时间
            update_session_activity(&state, &session_id).await;
            
            let chat_response = ChatResponse {
                session_id,
                response,
                status: "success".to_string(),
                error: None,
            };
            
            HttpResult::success(chat_response, trace_id)
        }
        Err(e) => {
            error!("AI command execution failed: {}", e);
            HttpResult::error(
                "AI001",
                &format!("AI command execution failed: {}", e),
                trace_id,
            )
        }
    }
}

/// 解析 multipart 请求数据
async fn parse_multipart_request(
    multipart: &mut Multipart,
    state: &SharedState,
) -> Result<MultipartChatRequest, anyhow::Error> {
    let mut prompt = String::new();
    let mut user_id = String::new();
    let mut project_id: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut files = Vec::new();
    let mut code_snippets = Vec::new();
    let mut code_references = Vec::new();

    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or("").to_string();
        
        match name.as_str() {
            "prompt" => {
                prompt = field.text().await?;
            }
            "user_id" => {
                user_id = field.text().await?;
            }
            "project_id" => {
                let value = field.text().await?;
                if !value.is_empty() {
                    project_id = Some(value);
                }
            }
            "session_id" => {
                let value = field.text().await?;
                if !value.is_empty() {
                    session_id = Some(value);
                }
            }
            "code_snippets" => {
                let json_str = field.text().await?;
                if !json_str.is_empty() {
                    let snippets: Vec<CodeSnippet> = serde_json::from_str(&json_str)?;
                    code_snippets.extend(snippets);
                }
            }
            "code_references" => {
                let json_str = field.text().await?;
                if !json_str.is_empty() {
                    let references: Vec<String> = serde_json::from_str(&json_str)?;
                    for ref_str in references {
                        if let Ok(uri) = ResourceUri::parse(&ref_str) {
                            code_references.push(uri);
                        }
                    }
                }
            }
            name if name.starts_with("files") => {
                // 处理文件上传
                if let Some(filename) = field.file_name() {
                    let filename = filename.to_string();
                    let content_type = field.content_type().unwrap_or("application/octet-stream").to_string();
                    let content = field.bytes().await?.to_vec();
                    let size = content.len();
                    
                    // 保存文件到项目目录
                    let saved_path = save_uploaded_file(&filename, &content, &project_id, state).await?;
                    
                    // 创建资源URI
                    let resource_uri = ResourceUriBuilder::file(&saved_path);
                    
                    let uploaded_file = UploadedFile {
                        filename: filename.clone(),
                        content_type,
                        content,
                        size,
                        resource_uri,
                    };
                    
                    info!("处理上传文件: {} ({} bytes)", filename, size);
                    files.push(uploaded_file);
                }
            }
            _ => {
                debug!("忽略未知字段: {}", name);
            }
        }
    }

    if prompt.is_empty() || user_id.is_empty() {
        return Err(anyhow::anyhow!("prompt 和 user_id 是必需的"));
    }

    Ok(MultipartChatRequest {
        prompt,
        user_id,
        project_id,
        session_id,
        files,
        code_snippets,
        code_references,
    })
}

/// 保存上传的文件
async fn save_uploaded_file(
    filename: &str,
    content: &[u8],
    project_id: &Option<String>,
    state: &SharedState,
) -> Result<PathBuf, anyhow::Error> {
    // 确定保存路径
    let base_dir = if let Some(project_id) = project_id {
        state.config.projects_dir.join(project_id)
    } else {
        state.config.projects_dir.join("uploads")
    };

    // 创建上传目录
    let upload_dir = base_dir.join("uploads");
    tokio::fs::create_dir_all(&upload_dir).await?;

    // 生成安全的文件名（避免路径遍历攻击）
    let safe_filename = sanitize_filename(filename);
    let file_path = upload_dir.join(&safe_filename);

    // 如果文件已存在，添加时间戳后缀
    let final_path = if file_path.exists() {
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let stem = file_path.file_stem().unwrap_or_default().to_string_lossy();
        let extension = file_path.extension().unwrap_or_default().to_string_lossy();
        
        if extension.is_empty() {
            upload_dir.join(format!("{}_{}", stem, timestamp))
        } else {
            upload_dir.join(format!("{}_{}.{}", stem, timestamp, extension))
        }
    } else {
        file_path
    };

    // 保存文件
    tokio::fs::write(&final_path, content).await?;
    info!("文件已保存到: {:?}", final_path);

    Ok(final_path)
}

/// 安全化文件名
fn sanitize_filename(filename: &str) -> String {
    filename
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 构建增强的 prompt，包含文件和代码片段信息
async fn build_enhanced_prompt(request: &MultipartChatRequest) -> Result<String, anyhow::Error> {
    let mut enhanced_prompt = String::new();
    
    // 添加原始 prompt
    enhanced_prompt.push_str(&request.prompt);
    enhanced_prompt.push_str("\n\n");

    // 添加文件信息
    if !request.files.is_empty() {
        enhanced_prompt.push_str("=== 上传的文件 ===\n");
        for file in &request.files {
            enhanced_prompt.push_str(&format!("文件: {} ({})\n", file.filename, file.content_type));
            enhanced_prompt.push_str(&format!("大小: {} bytes\n", file.size));
            enhanced_prompt.push_str(&format!("URI: {}\n", file.resource_uri.to_uri()));
            
            // 如果是文本文件，包含内容预览
            if is_text_file(&file.content_type) && file.size < 10000 { // 小于10KB的文本文件
                if let Ok(content) = String::from_utf8(file.content.clone()) {
                    enhanced_prompt.push_str("内容:\n```\n");
                    enhanced_prompt.push_str(&content);
                    enhanced_prompt.push_str("\n```\n");
                }
            }
            enhanced_prompt.push('\n');
        }
    }

    // 添加代码片段
    if !request.code_snippets.is_empty() {
        enhanced_prompt.push_str("=== 代码片段 ===\n");
        for (index, snippet) in request.code_snippets.iter().enumerate() {
            enhanced_prompt.push_str(&format!("代码片段 {}:\n", index + 1));
            
            if let Some(desc) = &snippet.description {
                enhanced_prompt.push_str(&format!("描述: {}\n", desc));
            }
            
            if let Some(file_path) = &snippet.file_path {
                enhanced_prompt.push_str(&format!("文件: {}\n", file_path));
            }
            
            if let Some((start, end)) = snippet.line_range {
                enhanced_prompt.push_str(&format!("行号: {}-{}\n", start, end));
            }
            
            let lang = snippet.language.as_deref().unwrap_or("");
            enhanced_prompt.push_str(&format!("```{}\n{}\n```\n\n", lang, snippet.content));
        }
    }

    // 添加代码引用
    if !request.code_references.is_empty() {
        enhanced_prompt.push_str("=== 代码引用 ===\n");
        for reference in &request.code_references {
            enhanced_prompt.push_str(&format!("引用: {} ({})\n", reference.name(), reference.to_uri()));
        }
        enhanced_prompt.push('\n');
    }

    Ok(enhanced_prompt)
}

/// 判断是否为文本文件
fn is_text_file(content_type: &str) -> bool {
    content_type.starts_with("text/") ||
    content_type == "application/json" ||
    content_type == "application/xml" ||
    content_type == "application/javascript" ||
    content_type == "application/typescript"
}

/// 为多媒体请求创建新会话
async fn create_new_session_for_multipart(state: &SharedState, request: &MultipartChatRequest) -> String {
    use crate::SessionInfo;
    
    let session_id = Uuid::new_v4().to_string();
    let session_info = SessionInfo {
        session_id: session_id.clone(),
        user_id: request.user_id.clone(),
        project_id: request.project_id.clone(),
        agent_type: state.config.default_agent.clone(),
        created_at: chrono::Utc::now(),
        last_activity: chrono::Utc::now(),
    };
    
    state.sessions.insert(session_id.clone(), session_info);
    
    info!("Created new session for multipart request: {}", session_id);
    session_id
}