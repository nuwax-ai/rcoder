//! 聊天处理器
//!
//! 将原始 HTTP 请求直接转发到容器内的 agent_runner 服务

use anyhow::Result;
use axum::{Json, extract::State};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use shared_types::{ModelProviderConfig, ProjectAndContainerInfo};
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{router::AppState, *};
use docker_manager::ContainerBasicInfo;

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
#[instrument(skip(state,request), fields(project_id = ?request.project_id, session_id = ?request.session_id))]
pub async fn handle_chat(
    State(state): State<Arc<AppState>>,
    Json(mut request): Json<ChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    let project_id = match &request.project_id {
        Some(id) => id.clone(),
        None => {
            let project_id = crate::service::container_manager::generate_project_id();
            request.project_id = Some(project_id.clone()); // 设置 project_id
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

    // 第二步：获取或创建 ProjectAndContainerInfo - 使用新的高效状态管理
    let project_info_ref = {
        info!("🔍 [CHAT] 开始获取/创建项目信息: project_id={}", project_id);

        // 使用 entry API 一次性处理获取和创建，避免多次锁获取
        let entry = state.project_and_agent_map.entry(project_id.clone());

        match entry {
            dashmap::mapref::entry::Entry::Occupied(mut occupied_entry) => {
                info!("📋 [CHAT] 更新现有项目信息: project_id={}", project_id);

                // 获取现有信息的可变引用，使用新的高效更新方法
                let existing_info = occupied_entry.get();

                // 检查是否需要更新扩展状态（避免不必要的写时复制）
                let needs_extended_update = existing_info.container().is_none()
                    || existing_info.model_provider().is_none()
                    || existing_info.request_id().is_none();

                if needs_extended_update {
                    // 使用新的批量更新方法，减少 Arc::make_mut 调用次数
                    let mut mutable_info = (**existing_info).clone();
                    mutable_info.update_extended_from_request(
                        Some(container_info.clone()),
                        request.model_provider.clone(),
                        request.request_id.clone(),
                    );
                    mutable_info.update_activity(); // 更新活动时间

                    let arc_info = Arc::new(mutable_info);
                    occupied_entry.insert(arc_info.clone());

                    info!(
                        "✅ [CHAT] 项目信息完整更新完成: project_id={}, container_id={}",
                        project_id, container_info.container_id
                    );

                    arc_info
                } else {
                    // 只需要更新活动时间
                    let mut mutable_info = (**existing_info).clone();
                    mutable_info.update_activity();

                    let arc_info = Arc::new(mutable_info);
                    occupied_entry.insert(arc_info.clone());

                    info!("✅ [CHAT] 项目活动时间更新完成: project_id={}", project_id);

                    arc_info
                }
            }
            dashmap::mapref::entry::Entry::Vacant(vacant_entry) => {
                info!("🆕 [CHAT] 创建新项目信息: project_id={}", project_id);

                // 创建新的 ProjectAndContainerInfo，使用新的初始化方法
                let mut new_info = ProjectAndContainerInfo::new(project_id.clone());
                new_info.update_extended_from_request(
                    Some(container_info.clone()),
                    request.model_provider.clone(),
                    request.request_id.clone(),
                );

                let arc_info = Arc::new(new_info);
                vacant_entry.insert(arc_info.clone());

                info!(
                    "✅ [CHAT] 项目信息创建完成: project_id={}, container_id={}",
                    project_id, container_info.container_id
                );

                arc_info
            }
        }
    };

    // 第三步：转发请求到容器服务
    info!("🚀 [CHAT] 开始转发请求到容器服务");
    let result = forward_request_to_container_service(&request, &container_info).await;
    info!("📥 [CHAT] 容器服务返回结果: success={}", result.is_ok());

    // 响应后状态更新 - 使用新的高效会话更新方法
    if let Ok(http_result) = &result {
        if let Some(chat_response) = &http_result.data {
            info!(
                "📊 [CHAT] 收到聊天响应，开始状态更新: session_id={}",
                chat_response.session_id
            );

            // 收集会话信息
            let session_id = chat_response.session_id.clone();

            // 使用新的高效更新方法进行原子性状态更新
            info!("🔄 [CHAT] 开始更新项目会话状态: project_id={}", project_id);

            let updated_arc_info = {
                let entry = state.project_and_agent_map.entry(project_id.clone());
                match entry {
                    dashmap::mapref::entry::Entry::Occupied(mut occupied_entry) => {
                        // 使用新的会话更新方法，自动处理写时复制
                        let existing_info = occupied_entry.get();

                        // 检查是否真的需要更新会话信息
                        if existing_info.session_id() != Some(&session_id) {
                            // 创建可变副本并更新会话信息
                            let mut mutable_info = (**existing_info).clone();
                            mutable_info.update_session(session_id.clone());

                            let arc_info = Arc::new(mutable_info);
                            occupied_entry.insert(arc_info.clone());

                            info!(
                                "✅ [CHAT] 项目会话状态更新完成: project_id={}, session_id={}",
                                project_id, session_id
                            );

                            arc_info
                        } else {
                            // 只需更新活动时间
                            let mut mutable_info = (**existing_info).clone();
                            mutable_info.update_activity();

                            let arc_info = Arc::new(mutable_info);
                            occupied_entry.insert(arc_info.clone());

                            info!(
                                "✅ [CHAT] 项目活动时间更新完成: project_id={}, session_id={}",
                                project_id, session_id
                            );

                            arc_info
                        }
                    }
                    dashmap::mapref::entry::Entry::Vacant(vacant_entry) => {
                        warn!(
                            "⚠️ [CHAT] 项目信息不存在，创建新条目: project_id={}",
                            project_id
                        );
                        let mut new_info = ProjectAndContainerInfo::new(project_id.clone());
                        new_info.update_session(session_id.clone());

                        let arc_info = Arc::new(new_info);
                        vacant_entry.insert(arc_info.clone());

                        info!(
                            "✅ [CHAT] 新项目会话状态创建完成: project_id={}, session_id={}",
                            project_id, session_id
                        );

                        arc_info
                    }
                }
            };

            // 更新 sessions 映射 - 使用轻量级索引
            info!("🔄 [CHAT] 更新会话索引映射: session_id={}", session_id);
            state
                .sessions
                .insert(session_id.clone(), updated_arc_info.clone());

            info!(
                "🎯 [CHAT] 所有状态更新完成: project_id={}, session_id={}",
                project_id, session_id
            );
        }
    } else {
        error!("❌ [CHAT] 容器服务返回错误: {:?}", result);
    }

    info!("🏁 [CHAT] 准备返回最终结果: project_id={}", project_id);
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
        // 先获取响应文本用于调试
        let response_text = response.text().await.map_err(|e| {
            error!("❌ [FORWARD] 读取容器响应文本失败: {}", e);
            crate::AppError::internal_server_error(&format!("读取容器响应失败: {}", e))
        })?;

        debug!("📥 [FORWARD] 容器响应原始内容: {}", response_text);

        // 解析容器的 HttpResult<ChatResponse> 响应
        let container_http_result: shared_types::HttpResult<ChatResponse> =
            serde_json::from_str(&response_text).map_err(|e| {
                error!("❌ [FORWARD] 解析容器响应失败: {}, 响应内容: {}", e, response_text);
                crate::AppError::internal_server_error(&format!("解析容器响应失败: {}", e))
            })?;

        debug!("📊 [FORWARD] 解析后的容器响应: {:?}", container_http_result);

        // 检查容器响应是否成功 - 优先检查 code 字段，因为这是业务逻辑标准
        if container_http_result.code == "0000" {
            // 成功情况：提取 data 字段中的 ChatResponse
            match container_http_result.data {
                Some(container_response) => {
                    info!(
                        "✅ [FORWARD] 容器响应成功: project_id={}, session_id={}",
                        container_response.project_id, container_response.session_id
                    );
                    Ok(crate::HttpResult::success(container_response))
                }
                None => {
                    error!("❌ [FORWARD] 成功响应但缺少 data 字段, 完整响应: {:?}", container_http_result);
                    Ok(crate::HttpResult::error("CONTAINER_ERROR", "成功响应但缺少 data 字段"))
                }
            }
        } else {
            // 错误情况：容器返回了错误响应
            let error_message = format!(
                "容器服务错误: code={}, message={}",
                container_http_result.code, container_http_result.message
            );
            error!("❌ [FORWARD] {}", error_message);
            Ok(crate::HttpResult::error("CONTAINER_ERROR", &error_message))
        }
    } else {
        let error_text = format!("容器返回错误状态: {}", status);
        let response_body = response.text().await.unwrap_or_default();
        error!("❌ [FORWARD] {}, 响应内容: {}", error_text, response_body);
        Ok(crate::HttpResult::error("CONTAINER_ERROR", &error_text))
    }
}
