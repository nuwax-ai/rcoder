//! 聊天处理器
//!
//! 将原始 HTTP 请求直接转发到容器内的 agent_runner 服务

use anyhow::Result;
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use shared_types::{ChatAgentConfig, ModelProviderConfig, ProjectAndContainerInfo};
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};
use utoipa::ToSchema;

use crate::{router::AppState, *};
use docker_manager::ContainerBasicInfo;

use super::utils::extract_grpc_addr_with_port;

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

    // === 新增字段 (v2) ===
    /// 可选的系统提示词，覆盖默认配置
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "你是一个专业的 Rust 开发者")]
    pub system_prompt: Option<String>,

    /// 可选的用户提示词模板，支持 {user_prompt} 变量替换
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "请用 Rust 完成：{user_prompt}")]
    pub user_prompt: Option<String>,

    /// 可选的 Agent 运行时配置（Agent 服务器 + MCP 服务器）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<ChatAgentConfig>,
}

/// 处理聊天请求 - 转发到容器化 agent_runner 服务
///
/// 1. 根据 project_id 检查或动态创建对应的容器（默认使用 ServiceType::RCoder）
/// 2. 将原始聊天请求直接转发到容器内的 agent_runner 服务
/// 3. 获取并返回 agent_runner 的处理结果
///
/// 注意：
/// - 所有参数处理（如 project_id、session_id 生成）都由 agent_runner 处理
/// - RCoder 只负责容器管理和请求转发
/// - 当前默认使用 ServiceType::RCoder，AgentRunner 模式正在开发中
/// - Resume 会话的降级逻辑已在 agent_runner 层通过 list_sessions API 预检查处理
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
            status = 401,
            description = "API Key 鉴权失败",
            body = String
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
    description = "根据 project_id 动态管理容器（默认使用 ServiceType::RCoder），将原始聊天请求直接转发到容器内的 agent_runner 服务进行处理"
)]
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

    // 验证资源限制配置
    if let Some(ref agent_config) = request.agent_config {
        if let Some(ref resource_limits) = agent_config.resource_limits {
            resource_limits.validate().map_err(|e| {
                AppError::validation_error(&format!("Invalid resource limits: {}", e))
            })?;
        }
    }

    info!(
        "🚀 [CHAT] 开始处理聊天请求: project_id={}, session_id={:?}, prompt_length={}, attachments_count={}, model_provider={}",
        project_id,
        request.session_id,
        request.prompt.len(),
        request.attachments.len(),
        request
            .model_provider
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "None".to_string())
    );

    // 打印 agent_config 配置信息（debug 级别）
    debug!(
        "🔧 [CHAT] agent_config 配置: project_id={}, agent_config={:?}",
        project_id, request.agent_config
    );

    // 第一步：获取或创建容器，默认使用 ServiceType::RCoder
    let service_type = shared_types::ServiceType::RCoder;
    let container_info =
        crate::service::container_manager::ContainerManager::get_or_create_container(
            &project_id,
            &service_type,
            request
                .agent_config
                .as_ref()
                .and_then(|c| c.resource_limits.clone()),
        )
        .await?;

    // 第二步：获取或创建 ProjectAndContainerInfo - 使用 DuckDB 存储
    let _ = {
        info!("🔍 [CHAT] 开始获取/创建项目信息: project_id={}", project_id);

        // 检查项目是否存在
        if let Some(existing_info) = state.get_project(&project_id) {
            info!("📋 [CHAT] 更新现有项目信息: project_id={}", project_id);

            // 检查是否需要更新扩展状态
            let needs_extended_update = existing_info.container().is_none()
                || existing_info.model_provider().is_none()
                || existing_info.request_id().is_none();

            if needs_extended_update {
                // 创建更新后的信息
                let mut mutable_info = (*existing_info).clone();
                mutable_info.update_extended_from_request(
                    Some(container_info.clone()),
                    request.model_provider.clone(),
                    request.request_id.clone(),
                    Some(service_type.clone()),
                );
                mutable_info.update_activity();

                let arc_info = Arc::new(mutable_info);
                state.insert_project(project_id.clone(), arc_info.clone());

                info!(
                    "✅ [CHAT] 项目信息完整更新完成: project_id={}, container_id={}",
                    project_id, container_info.container_id
                );

                arc_info
            } else {
                // 只需要更新活动时间
                state.update_activity(&project_id);
                info!("✅ [CHAT] 项目活动时间更新完成: project_id={}", project_id);
                existing_info
            }
        } else {
            info!("🆕 [CHAT] 创建新项目信息: project_id={}", project_id);

            // 创建新的 ProjectAndContainerInfo
            let mut new_info = ProjectAndContainerInfo::new(project_id.clone());
            new_info.update_extended_from_request(
                Some(container_info.clone()),
                request.model_provider.clone(),
                request.request_id.clone(),
                Some(service_type.clone()),
            );

            let arc_info = Arc::new(new_info);
            state.insert_project(project_id.clone(), arc_info.clone());

            info!(
                "✅ [CHAT] 项目信息创建完成: project_id={}, container_id={}",
                project_id, container_info.container_id
            );

            arc_info
        }
    };

    // 请求到达时立即更新活动时间（不等待请求执行结果）
    // 这样可以防止在 gRPC 请求期间被 cleanup_task 误清理
    state.update_activity(&project_id);
    debug!("🔄 [CHAT] 已更新活动时间: project_id={}", project_id);

    // 🆕 自动查找 session_id 逻辑
    // 如果用户没有传递 session_id，尝试从状态中查找最新的 session_id
    let session_id_to_use = match &request.session_id {
        Some(sid) if !sid.is_empty() => {
            debug!("🔍 [CHAT] 使用用户传递的 session_id: {}", sid);
            sid.clone()
        }
        _ => {
            // 用户没有传递 session_id，尝试查找最新的
            match state.get_project(&project_id) {
                Some(project_info) => {
                    let existing_session_id = project_info.session_id();
                    match existing_session_id {
                        Some(sid) if !sid.is_empty() => {
                            info!(
                                "🔄 [CHAT] 未传递 session_id，自动使用最新会话: project_id={}, session_id={}",
                                project_id, sid
                            );
                            sid.to_string()
                        }
                        _ => {
                            debug!("🆕 [CHAT] 项目存在但无 session_id，创建新会话");
                            String::new()
                        }
                    }
                }
                None => {
                    debug!("🆕 [CHAT] 未找到现有项目，创建新会话");
                    String::new()
                }
            }
        }
    };

    // 克隆 request 并修改 session_id
    let mut request_for_forward = request.clone();
    request_for_forward.session_id = if session_id_to_use.is_empty() {
        None
    } else {
        Some(session_id_to_use)
    };
    // 🆕 自动查找 session_id 逻辑结束

    // 第三步：转发请求到容器服务（使用全局连接池）
    info!("🚀 [CHAT] 开始转发请求到容器服务");
    let result = forward_request_to_container_service(
        &request_for_forward,
        &container_info,
        &state.grpc_pool,
    )
    .await;
    info!("📥 [CHAT] 容器服务返回结果: success={}", result.is_ok());

    // 响应后状态更新 - 使用 DuckDB 存储
    // 无论请求成功还是失败，只要响应中包含 session_id，都要更新映射
    // 这样用户可以通过 SSE 接口获取错误通知，而不会收到 SESSION_EXPIRED 错误
    if let Ok(http_result) = &result {
        if let Some(chat_response) = &http_result.data {
            let session_id = chat_response.session_id.clone();

            // 只有当 session_id 非空时才更新映射
            if !session_id.is_empty() {
                info!(
                    "📊 [CHAT] 收到聊天响应，开始状态更新: session_id={}, success={}",
                    session_id,
                    http_result.is_success()
                );

                // 更新会话信息（同时更新 session_id 和 session-to-container 映射）
                info!(
                    "🔗 [SESSION_MAP] 关联 session_id {} 到 project_id {}",
                    session_id, project_id
                );
                state.update_session(&project_id, &session_id);

                // 更新项目活动时间
                state.update_activity(&project_id);

                if http_result.is_success() {
                    info!(
                        "🎯 [CHAT] 所有状态更新完成: project_id={}, session_id={}",
                        project_id, session_id
                    );
                } else {
                    warn!(
                        "⚠️ [CHAT] 请求失败但已保存会话映射: project_id={}, session_id={}, code={}, message={}",
                        project_id, session_id, http_result.code, http_result.message
                    );
                }
            }
        }
    }

    if result.as_ref().map_or(true, |r| {
        !r.is_success() && r.data.as_ref().map_or(true, |d| d.session_id.is_empty())
    }) {
        error!("❌ [CHAT] 容器服务返回错误: {:?}", result);
    }

    info!("🏁 [CHAT] 准备返回最终结果: project_id={}", project_id);

    result
}

/// 转发请求到容器内的 agent_runner 服务
///
/// 🎯 使用 gRPC Chat RPC 替代 HTTP 转发（使用全局连接池）
async fn forward_request_to_container_service(
    request: &ChatRequest,
    container_info: &ContainerBasicInfo,
    grpc_pool: &Arc<crate::grpc::GrpcChannelPool>,
) -> Result<crate::HttpResult<ChatResponse>, crate::AppError> {
    let project_id = if let Some(id) = &request.project_id {
        id.clone()
    } else {
        error!("[FORWARD]会话 project_id 不能为空");
        return Ok(crate::HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id 不能为空",
        ));
    };

    info!(
        "📤 [FORWARD] 转发请求到容器 (gRPC): project_id={}, session_id={:?}, container_id={}, service_url={}",
        project_id, request.session_id, container_info.container_id, container_info.service_url
    );

    // 🎯 使用 gRPC 替代 HTTP
    // 从 service_url 提取主机名
    let grpc_addr =
        extract_grpc_addr_with_port(&container_info.service_url, shared_types::GRPC_DEFAULT_PORT)?;

    debug!(
        "📡 [FORWARD] 发送 gRPC 请求到: {}, prompt_length={}, attachments_count={}",
        grpc_addr,
        request.prompt.len(),
        request.attachments.len()
    );

    // 调用 gRPC Chat（使用全局连接池，带重试和被动驱逐机制）
    let max_retries = 2;
    let mut last_error = None;

    for attempt in 1..=max_retries {
        match crate::grpc::grpc_chat_with_pool(
            grpc_pool,
            &grpc_addr,
            project_id.clone(),
            request.session_id.clone(),
            request.prompt.clone(),
            request.attachments.clone(),
            request.data_source_attachments.clone(),
            request.model_provider.clone(),
            request.request_id.clone(),
            None, // ✅ 使用连接级别默认超时，未来可根据需要设置
            // 新增参数 (v2)
            request.system_prompt.clone(),
            request.user_prompt.clone(),
            request.agent_config.clone(),
            Some(shared_types::ServiceType::RCoder), // ✅ RCoder 模式使用 RCoder ServiceType
            None,                                    // RCoder 模式不需要 user_id
        )
        .await
        {
            Ok(grpc_response) => {
                if grpc_response.success {
                    // 转换为内部 ChatResponse
                    let chat_response = crate::grpc::grpc_response_to_chat_response(grpc_response);
                    info!(
                        "✅ [FORWARD] gRPC 响应成功: project_id={}, session_id={}",
                        chat_response.project_id, chat_response.session_id
                    );
                    return Ok(crate::HttpResult::success(chat_response));
                } else {
                    let error_msg = grpc_response
                        .error
                        .unwrap_or_else(|| "未知错误".to_string());
                    // 🎯 从 gRPC 响应中提取错误码（完整透传）
                    let error_code = grpc_response
                        .error_code
                        .unwrap_or_else(|| shared_types::error_codes::ERR_AGENT_ERROR.to_string());
                    error!(
                        "❌ [FORWARD] gRPC 响应错误: code={}, message={}",
                        error_code, error_msg
                    );
                    return Ok(crate::HttpResult::error(&error_code, &error_msg));
                }
            }
            Err(e) => {
                warn!(
                    "⚠️ [FORWARD] gRPC 调用失败 (第 {}/{} 次): {}",
                    attempt, max_retries, e
                );

                // ✅ 使用错误分类判断是否应该重试
                let should_retry = crate::grpc::should_retry_error(&e);

                if should_retry && attempt < max_retries {
                    // 可重试错误：清理连接池并重试
                    info!(
                        "🔄 [FORWARD] 检测到可重试错误，从连接池移除 {} 并重试...",
                        grpc_addr
                    );
                    grpc_pool.remove(&grpc_addr);
                    last_error = Some(e);
                    continue;
                } else if !should_retry {
                    // 不可重试错误：直接返回
                    error!("❌ [FORWARD] 检测到不可重试错误，停止重试: {}", e);
                    last_error = Some(e);
                    break;
                }

                // 最后一次尝试失败
                last_error = Some(e);
            }
        }
    }

    // 如果所有重试都失败
    if let Some(e) = last_error {
        error!("❌ [FORWARD] gRPC 最终调用失败: {}", e);

        // gRPC 通信失败，直接返回错误
        // 注：业务错误码（如 Agent busy）现在由 agent_runner 通过 grpc_response.error_code 返回
        // 这里只处理真正的 gRPC 通信层错误
        Ok(HttpResult::error(
            shared_types::error_codes::ERR_GRPC_ERROR,
            &format!("gRPC 调用失败: {}", e),
        ))
    } else {
        // 理论上不会走到这里，除非 max_retries < 1
        Ok(HttpResult::error(
            shared_types::error_codes::ERR_GRPC_ERROR,
            "未知重试错误",
        ))
    }
}
