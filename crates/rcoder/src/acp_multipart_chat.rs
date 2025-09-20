use crate::{SharedState, ProgressEvent, ProgressEventType, broadcast_progress_event, get_trace_id, HttpResult, ChatResponse, SessionInfo, AgentType, AppConfig};
use acp_adapter::mention::{ResourceUri, ResourceUriBuilder};
use acp_adapter::plan::{PlanManager, PlanEvent, PlanUpdateEvent, PlanConverter};
use acp_adapter::types::{Plan, PlanEntry, PlanEntryStatus, PlanEntryPriority};
use agent_client_protocol::{
    PromptRequest, ContentBlock, TextContent, SessionId
};
use axum::{
    extract::{State, Multipart},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn, error, debug};
use uuid::Uuid;
use anyhow::Result;

/// 增强的多媒体聊天请求结构 - 基于ACP原生内容块
#[derive(Debug)]
pub struct AcpMultipartChatRequest {
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

/// 处理包含文件上传的聊天请求 - 使用ACP原生内容块
pub async fn handle_acp_multipart_chat(
    State(state): State<SharedState>,
    mut multipart: Multipart,
) -> HttpResult<ChatResponse> {
    let trace_id = get_trace_id();
    
    info!("收到ACP多媒体聊天请求, trace_id={:?}", trace_id);

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
        "解析的ACP多媒体请求: user_id={}, project_id={:?}, session_id={:?}, files_count={}, snippets_count={}, references_count={}",
        request.user_id, request.project_id, request.session_id, 
        request.files.len(), request.code_snippets.len(), request.code_references.len()
    );

    // 权限检查 - 根据用户记忆中的规范
    if let Err(e) = check_multipart_permissions(&request, &state).await {
        error!("权限检查失败: {}", e);
        return HttpResult::error(
            "PERMISSION001",
            &format!("权限检查失败: {}", e),
            trace_id,
        );
    }

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
                create_new_session_for_acp_multipart(&state, &request).await
            }
        }
        None => create_new_session_for_acp_multipart(&state, &request).await,
    };

    // 构建ACP原生内容块 - 这是核心改进
    let content_blocks = match build_acp_content_blocks(&request).await {
        Ok(blocks) => blocks,
        Err(e) => {
            error!("构建ACP内容块失败: {}", e);
            return HttpResult::error(
                "ACP001",
                &format!("构建ACP内容块失败: {}", e),
                trace_id,
            );
        }
    };

    // 创建ACP PromptRequest
    let acp_request = PromptRequest {
        session_id: SessionId(session_id.clone().into()),
        prompt: content_blocks,
        meta: None,
    };

    // 获取会话信息以确定使用的代理类型
    let agent_type = {
        state.sessions.get(&session_id)
            .map(|s| s.agent_type.clone())
            .unwrap_or(state.config.default_agent.clone())
    };

    // 通过ACP协议发送原生内容块到AI代理
    match execute_acp_command(&agent_type, &acp_request, &state.config, &state, &session_id).await {
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
            error!("ACP command execution failed: {}", e);
            HttpResult::error(
                "ACP002",
                &format!("ACP command execution failed: {}", e),
                trace_id,
            )
        }
    }
}

/// 构建ACP原生内容块 - 核心改进点
async fn build_acp_content_blocks(request: &AcpMultipartChatRequest) -> Result<Vec<ContentBlock>> {
    let mut content_blocks = Vec::new();
    
    // 1. 添加主要的文本prompt
    content_blocks.push(ContentBlock::Text(TextContent {
        text: request.prompt.clone(),
        annotations: None,
        meta: None,
    }));
    
    // 2. 添加上传的文件信息描述（简化版本）
    if !request.files.is_empty() {
        let mut file_descriptions = Vec::new();
        for file in &request.files {
            file_descriptions.push(format!(
                "📄 文件: {} ({}, {})", 
                file.filename, 
                file.content_type,
                format_file_size(file.size)
            ));
            
            // 如果是小的文本文件，添加内容
            if is_text_file(&file.content_type) && file.size < 50000 {
                if let Ok(text_content) = String::from_utf8(file.content.clone()) {
                    file_descriptions.push(format!("内容:\n```\n{}\n```", text_content));
                }
            }
        }
        
        content_blocks.push(ContentBlock::Text(TextContent {
            text: format!("上传的文件:\n{}", file_descriptions.join("\n\n")),
            annotations: None,
            meta: None,
        }));
    }
    
    // 3. 添加代码片段
    if !request.code_snippets.is_empty() {
        let mut snippet_descriptions = Vec::new();
        for (index, snippet) in request.code_snippets.iter().enumerate() {
            let snippet_name = snippet.description.clone()
                .unwrap_or_else(|| format!("代码片段 {}", index + 1));
            
            let language_tag = snippet.language.as_deref().unwrap_or("");
            snippet_descriptions.push(format!(
                "📝 {}:\n```{}\n{}\n```", 
                snippet_name, 
                language_tag,
                snippet.content
            ));
        }
        
        content_blocks.push(ContentBlock::Text(TextContent {
            text: format!("代码片段:\n{}", snippet_descriptions.join("\n\n")),
            annotations: None,
            meta: None,
        }));
    }
    
    // 4. 添加代码引用信息
    if !request.code_references.is_empty() {
        let mut reference_descriptions = Vec::new();
        for reference in &request.code_references {
            reference_descriptions.push(format!(
                "🔗 代码引用: {}", 
                reference.name()
            ));
        }
        
        content_blocks.push(ContentBlock::Text(TextContent {
            text: format!("代码引用:\n{}", reference_descriptions.join("\n")),
            annotations: None,
            meta: None,
        }));
    }

    info!("构建了 {} 个ACP内容块", content_blocks.len());
    Ok(content_blocks)
}

/// 权限检查 - 基于用户记忆中的权限管理规范
async fn check_multipart_permissions(
    request: &AcpMultipartChatRequest, 
    state: &SharedState
) -> Result<()> {
    // 检查文件上传权限
    if !request.files.is_empty() {
        info!("检查文件上传权限: {} 个文件", request.files.len());
        
        // 这里应该集成PermissionManager进行权限检查
        // 但由于当前架构限制，我们先做基础检查
        for file in &request.files {
            if file.size > 10 * 1024 * 1024 { // 10MB限制
                return Err(anyhow::anyhow!("文件 {} 超过大小限制 (10MB)", file.filename));
            }
            
            // 检查文件类型是否允许
            if is_dangerous_file_type(&file.content_type) {
                return Err(anyhow::anyhow!("不允许的文件类型: {}", file.content_type));
            }
        }
    }
    
    // 检查代码片段权限
    if !request.code_snippets.is_empty() {
        info!("检查代码片段权限: {} 个片段", request.code_snippets.len());
        
        for snippet in &request.code_snippets {
            if snippet.content.len() > 100_000 { // 100KB限制
                return Err(anyhow::anyhow!("代码片段过大"));
            }
        }
    }
    
    Ok(())
}

/// 判断是否为危险文件类型
fn is_dangerous_file_type(content_type: &str) -> bool {
    matches!(content_type, 
        "application/x-executable" | 
        "application/x-msdownload" |
        "application/x-msdos-program" |
        "application/x-winexe"
    )
}

/// 判断是否为文本文件
fn is_text_file(content_type: &str) -> bool {
    content_type.starts_with("text/") ||
    content_type == "application/json" ||
    content_type == "application/xml" ||
    content_type == "application/javascript" ||
    content_type == "application/typescript" ||
    content_type.contains("yaml") ||
    content_type.contains("toml")
}

/// 格式化文件大小
fn format_file_size(bytes: usize) -> String {
    if bytes == 0 { return "0 B".to_string(); }
    let units = ["B", "KB", "MB", "GB"];
    let base = 1024_f64;
    let log = (bytes as f64).log(base).floor() as usize;
    let unit_index = log.min(units.len() - 1);
    let size = bytes as f64 / base.powi(unit_index as i32);
    
    if size.fract() == 0.0 {
        format!("{:.0} {}", size, units[unit_index])
    } else {
        format!("{:.1} {}", size, units[unit_index])
    }
}

/// 解析 multipart 请求数据 - 复用之前的实现
async fn parse_multipart_request(
    multipart: &mut Multipart,
    state: &SharedState,
) -> Result<AcpMultipartChatRequest> {
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

    Ok(AcpMultipartChatRequest {
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
) -> Result<PathBuf> {
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

/// 为ACP多媒体请求创建新会话
async fn create_new_session_for_acp_multipart(state: &SharedState, request: &AcpMultipartChatRequest) -> String {
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
    
    info!("Created new session for ACP multipart request: {}", session_id);
    session_id
}

/// 执行ACP命令 - 需要实现具体的ACP协议调用
async fn execute_acp_command(
    agent_type: &crate::AgentType,
    acp_request: &PromptRequest,
    config: &crate::AppConfig,
    state: &SharedState,
    session_id: &str,
) -> Result<String> {
    // 发送任务开始事件
    let start_event = ProgressEvent {
        event_type: ProgressEventType::TaskStarted,
        message: format!("开始执行 ACP 任务: {}", agent_type),
        timestamp: chrono::Utc::now(),
        session_id: session_id.to_string(),
        data: Some(serde_json::json!({
            "agent_type": agent_type.to_string(),
            "content_blocks_count": acp_request.prompt.len()
        })),
    };
    broadcast_progress_event(state, session_id, start_event);

    // TODO: 这里需要集成真正的ACP代理调用
    // 目前先返回模拟响应，展示ACP内容块的结构
    let mut response_parts = Vec::new();
    response_parts.push(format!("基于ACP协议处理了您的多媒体请求："));
    
    for (index, block) in acp_request.prompt.iter().enumerate() {
        match block {
            ContentBlock::Text(text_block) => {
                response_parts.push(format!("📝 文本内容 {}: {}", index + 1, 
                    if text_block.text.len() > 100 {
                        format!("{}...", &text_block.text[..100])
                    } else {
                        text_block.text.clone()
                    }
                ));
            }
            _ => {
                response_parts.push(format!("🔧 其他内容块 {}", index + 1));
            }
        }
    }

    let response = response_parts.join("\n");

    // 发送任务完成事件
    let complete_event = ProgressEvent {
        event_type: ProgressEventType::TaskCompleted,
        message: "ACP任务执行完成".to_string(),
        timestamp: chrono::Utc::now(),
        session_id: session_id.to_string(),
        data: Some(serde_json::json!({
            "success": true,
            "response_length": response.len()
        })),
    };
    broadcast_progress_event(state, session_id, complete_event);

    Ok(response)
}

/// 更新会话活动时间 - 复用现有实现
async fn update_session_activity(state: &SharedState, session_id: &str) {
    if let Some(mut session) = state.sessions.get_mut(session_id) {
        session.last_activity = chrono::Utc::now();
    }
}