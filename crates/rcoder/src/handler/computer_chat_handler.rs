//! Computer Agent Runner 聊天处理器
//!
//! 处理 Computer Agent Runner 模式的聊天请求。
//! 与 RCoder 的 project_id 容器模式不同，ComputerAgentRunner 使用 user_id 作为容器标识。
//!
//! ## 请求流程
//! ```text
//! POST /computer/chat { user_id, project_id?, prompt, ... }
//!     ↓
//! 1. 验证 user_id
//! 2. 生成 project_id（若未提供）
//! 3. get_or_create_container_for_user(user_id)
//!    - 挂载配置: config.yml mounts (配置化管理)
//!    - 宿主机: /computer-project-workspace/{user_id} → 容器: /home/user
//! 4. 创建项目工作目录: /home/user/{project_id} (通过挂载自动同步)
//! 5. 创建/更新项目和会话信息
//! 6. gRPC Chat RPC → agent_runner (带 project_id)
//! 7. 更新会话映射
//! 8. 返回 ChatResponse
//! ```

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use shared_types::{ChatAgentConfig, ChatResponse, ModelProviderConfig};
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};
use utoipa::ToSchema;

use crate::{AppError, HttpResult, router::AppState, service::ComputerContainerManager};
use docker_manager::ContainerBasicInfo;
use shared_types::Attachment;

/// Computer Agent 聊天请求
///
/// 与标准 ChatRequest 的主要区别：
/// - `user_id` 是必填字段（用于容器标识）
/// - 一个 user_id 对应一个容器，容器内可以有多个 project_id 的 Agent 实例
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct ComputerChatRequest {
    /// 用户 ID (必填) - 一个用户对应一个容器
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目 ID (可选) - 一个容器内可以有多个项目
    /// 若未提供，系统自动生成 UUID
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 用户输入的 prompt
    #[schema(example = "帮我打开浏览器访问 https://example.com")]
    pub prompt: String,

    /// 可选的会话 ID，如果不提供则创建新会话
    #[schema(example = "session789")]
    pub session_id: Option<String>,

    /// 可选的附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,

    /// 数据源附件列表 - 用于AI开发时获取外部数据源信息
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_source_attachments: Vec<String>,

    /// 模型配置
    pub model_provider: Option<ModelProviderConfig>,

    /// 可选的请求ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "req_123456789")]
    pub request_id: Option<String>,

    /// 可选的系统提示词覆盖
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,

    /// 可选的用户提示词模板
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,

    /// Agent 运行时配置
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<ChatAgentConfig>,
}

/// 处理 Computer Agent 聊天请求
///
/// 1. 根据 user_id 获取或创建用户容器
/// 2. 将聊天请求转发到容器内的 agent_runner 服务
/// 3. 更新会话映射
///
/// 注意：
/// - user_id 是必填的，用于标识用户的容器
/// - project_id 可选，若未提供则自动生成
/// - 一个用户容器内可以运行多个 project_id 的 Agent 实例
#[utoipa::path(
    post,
    path = "/computer/chat",
    request_body(
        content = ComputerChatRequest,
        description = "Computer Agent 聊天请求，包含 user_id 和 prompt",
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
                    "project_id": "proj_456",
                    "session_id": "session789",
                    "error": null,
                    "request_id": "req_123456789"
                },
                "error": null
            })
        ),
        (
            status = 400,
            description = "请求参数错误（如 user_id 为空）",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "computer",
    operation_id = "handle_computer_chat",
    summary = "发送聊天消息到 Computer Agent",
    description = "根据 user_id 动态管理容器，一个用户对应一个带桌面环境的容器"
)]
#[axum::debug_handler]
#[instrument(skip(state, request), fields(user_id = %request.user_id, project_id = ?request.project_id))]
pub async fn handle_computer_chat(
    State(state): State<Arc<AppState>>,
    Json(mut request): Json<ComputerChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    // 1. 验证 user_id
    if request.user_id.trim().is_empty() {
        error!("❌ [COMPUTER_CHAT] user_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id 不能为空",
        ));
    }

    let user_id = request.user_id.clone();

    // 2. 生成或使用提供的 project_id
    let project_id = match &request.project_id {
        Some(id) if !id.trim().is_empty() => id.clone(),
        _ => {
            let generated_id = crate::service::container_manager::generate_project_id();
            request.project_id = Some(generated_id.clone());
            generated_id
        }
    };

    info!(
        "🚀 [COMPUTER_CHAT] 开始处理请求: user_id={}, project_id={}, prompt_len={}, attachments={}",
        user_id,
        project_id,
        request.prompt.len(),
        request.attachments.len()
    );

    // 3. 验证资源限制配置
    if let Some(ref agent_config) = request.agent_config {
        if let Some(ref resource_limits) = agent_config.resource_limits {
            if let Err(e) = resource_limits.validate() {
                error!("❌ [COMPUTER_CHAT] 资源限制验证失败: {}", e);
                return Ok(HttpResult::error(
                    shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
                    &format!("Invalid resource limits: {}", e),
                ));
            }
        }
    }

    // 4. 获取或创建用户容器
    let container_info = match ComputerContainerManager::get_or_create_container_for_user(
        &user_id,
        request
            .agent_config
            .as_ref()
            .and_then(|c| c.resource_limits.clone()),
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            error!("❌ [COMPUTER_CHAT] 获取或创建容器失败: {}", e);
            return Ok(HttpResult::error(
                shared_types::error_codes::ERR_CONTAINER_ERROR,
                &format!("获取或创建容器失败: {}", e),
            ));
        }
    };

    info!(
        "✅ [COMPUTER_CHAT] 容器就绪: user_id={}, container_id={}, ip={}",
        user_id, container_info.container_id, container_info.container_ip
    );

    // 5. 创建项目工作目录（在用户容器内）
    // Computer Agent Runner 需要在用户工作区内为 project_id 创建子目录
    if let Err(e) =
        ensure_project_workspace_exists(&user_id, &project_id, &container_info.container_ip).await
    {
        error!("❌ [COMPUTER_CHAT] 创建项目工作目录失败: {}", e);
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_WORKSPACE_ERROR,
            &format!("创建项目工作目录失败: {}", e),
        ));
    }

    // 6. 注册 VNC 后端到 Pingora（用于 WebSocket 代理）
    if let Some(ref pingora_service) = state.pingora_service {
        pingora_service.add_vnc_backend(&user_id, &container_info.container_ip);
        debug!(
            "🔗 [COMPUTER_CHAT] VNC 后端已注册: user_id={} -> {}",
            user_id, container_info.container_ip
        );
    }

    // 6. 转发请求到容器服务（使用 gRPC）
    let result = forward_computer_request_to_container(
        &request,
        &project_id,
        &container_info,
        &state.grpc_pool,
    )
    .await;

    // 7. 更新会话映射（填充所有三个映射表，保持一致性）
    // 只有在成功时才更新映射表
    if result.is_success() {
        if let Some(chat_response) = &result.data {
            let session_id = chat_response.session_id.clone();
            let container_id = container_info.container_id.clone();

            info!(
                "🔗 [COMPUTER_CHAT] 关联会话: session_id={} -> container_id={}, user_id={}, project_id={}",
                session_id, container_id, user_id, project_id
            );

            // 🔧 ComputerAgentRunner 模式：使用 user_id 作为容器标识（一个用户一个容器）
            // 检查是否已存在该 user_id 的记录，如果存在则更新 last_activity
            let map_key = user_id.clone();

            // 使用 Entry API 实现原子性更新或插入
            use dashmap::mapref::entry::Entry;
            match state.project_and_agent_map.entry(map_key.clone()) {
                Entry::Occupied(mut occupied) => {
                    // ✅ 已存在：更新 last_activity 和 session_id（写时复制）
                    let old_info = occupied.get();
                    let mut updated_info = (*old_info).as_ref().clone();

                    // 更新活动时间（关键修复点！）
                    updated_info.update_activity();
                    updated_info.update_session(session_id.clone());

                    // 更新扩展信息
                    updated_info.update_extended_from_request(
                        Some(container_info.clone()),
                        request.model_provider.clone(),
                        request.request_id.clone(),
                        Some(shared_types::ServiceType::ComputerAgentRunner),
                    );

                    let updated_arc = Arc::new(updated_info);
                    occupied.insert(updated_arc.clone());

                    // 更新其他映射表
                    state
                        .session_to_container_id
                        .insert(session_id.clone(), container_id);
                    state.sessions.insert(session_id.clone(), updated_arc);

                    info!(
                        "🔄 [COMPUTER_CHAT] 已更新现有容器映射: user_id={}, project_id={}, session_id={} (last_activity 已刷新)",
                        user_id, project_id, session_id
                    );
                }
                Entry::Vacant(vacant) => {
                    // 🆕 不存在：创建新的 ProjectAndContainerInfo
                    let mut project_info =
                        shared_types::ProjectAndContainerInfo::new(map_key.clone());

                    // 设置 user_id（ComputerAgentRunner 模式）
                    project_info.set_user_id(Some(user_id.clone()));

                    // 更新会话ID
                    project_info.update_session(session_id.clone());

                    // 更新扩展信息（容器、模型配置等）
                    project_info.update_extended_from_request(
                        Some(container_info.clone()),
                        request.model_provider.clone(),
                        request.request_id.clone(),
                        Some(shared_types::ServiceType::ComputerAgentRunner),
                    );

                    let project_info_arc = Arc::new(project_info);
                    vacant.insert(project_info_arc.clone());

                    // 填充其他映射表
                    state
                        .session_to_container_id
                        .insert(session_id.clone(), container_id);
                    state.sessions.insert(session_id.clone(), project_info_arc);

                    info!(
                        "🆕 [COMPUTER_CHAT] 已创建新容器映射: user_id={}, project_id={}, session_id={}",
                        user_id, project_id, session_id
                    );
                }
            }

            info!(
                "✅ [COMPUTER_CHAT] 请求处理完成: user_id={}, project_id={}, session_id={} (所有映射表已更新)",
                user_id, project_id, session_id
            );
        }
    } else {
        error!(
            "❌ [COMPUTER_CHAT] 容器服务返回错误: user_id={}, project_id={}, code={}, message={}",
            user_id, project_id, result.code, result.message
        );
    }

    Ok(result)
}

/// 转发请求到容器内的 agent_runner 服务（仅使用 gRPC）
///
/// 与 RCoder 的 forward_request_to_container_service 类似，
/// 但专门用于 ComputerAgentRunner 模式。
async fn forward_computer_request_to_container(
    request: &ComputerChatRequest,
    project_id: &str,
    container_info: &ContainerBasicInfo,
    grpc_pool: &Arc<crate::grpc::GrpcChannelPool>,
) -> HttpResult<ChatResponse> {
    info!(
        "📤 [COMPUTER_FORWARD] 转发请求到容器 (gRPC): user_id={}, project_id={}, container_id={}",
        request.user_id, project_id, container_info.container_id
    );

    // 从 service_url 提取 gRPC 地址
    let grpc_addr =
        match extract_grpc_addr(&container_info.service_url, shared_types::GRPC_DEFAULT_PORT) {
            Ok(addr) => addr,
            Err(e) => {
                error!("❌ [COMPUTER_FORWARD] 提取 gRPC 地址失败: {}", e);
                return HttpResult::error(
                    shared_types::error_codes::ERR_GRPC_ADDR_ERROR,
                    &format!("提取 gRPC 地址失败: {}", e),
                );
            }
        };

    debug!(
        "📡 [COMPUTER_FORWARD] gRPC 地址: {}, prompt_len={}, attachments={}",
        grpc_addr,
        request.prompt.len(),
        request.attachments.len()
    );

    // Computer Agent Runner 的工作目录路径
    // 在容器内：/app/computer-project-workspace/{user_id}/{project_id}
    let project_workspace = format!(
        "/app/computer-project-workspace/{}/{}/",
        request.user_id, project_id
    );

    debug!("📁 [COMPUTER_FORWARD] 项目工作目录: {}", project_workspace);

    // gRPC 调用（带重试机制）
    let max_retries = 2;
    let mut last_error = None;

    for attempt in 1..=max_retries {
        match crate::grpc::grpc_chat_with_pool(
            grpc_pool,
            &grpc_addr,
            project_id.to_string(),
            request.session_id.clone(),
            request.prompt.clone(),
            request.attachments.clone(),
            request.data_source_attachments.clone(),
            request.model_provider.clone(),
            request.request_id.clone(),
            None, // 使用默认超时
            request.system_prompt.clone(),
            request.user_prompt.clone(),
            request.agent_config.clone(),
            Some(shared_types::ServiceType::ComputerAgentRunner), // ✅ 传递正确的 ServiceType
            Some(request.user_id.clone()), // ✅ 传递 user_id（ComputerAgentRunner 必需）
        )
        .await
        {
            Ok(grpc_response) => {
                if grpc_response.success {
                    let chat_response = crate::grpc::grpc_response_to_chat_response(grpc_response);
                    info!(
                        "✅ [COMPUTER_FORWARD] gRPC 响应成功: project_id={}, session_id={}",
                        chat_response.project_id, chat_response.session_id
                    );
                    return HttpResult::success(chat_response);
                } else {
                    let error_msg = grpc_response
                        .error
                        .unwrap_or_else(|| "未知错误".to_string());
                    // 🎯 从 gRPC 响应中提取错误码（完整透传）
                    let error_code = grpc_response
                        .error_code
                        .unwrap_or_else(|| shared_types::error_codes::ERR_AGENT_ERROR.to_string());
                    error!(
                        "❌ [COMPUTER_FORWARD] gRPC 响应错误: code={}, message={}",
                        error_code, error_msg
                    );
                    return HttpResult::error(&error_code, &error_msg);
                }
            }
            Err(e) => {
                warn!(
                    "⚠️ [COMPUTER_FORWARD] gRPC 调用失败 (第 {}/{} 次): {}",
                    attempt, max_retries, e
                );

                let should_retry = crate::grpc::should_retry_error(&e);

                if should_retry && attempt < max_retries {
                    info!(
                        "🔄 [COMPUTER_FORWARD] 检测到可重试错误，从连接池移除 {} 并重试...",
                        grpc_addr
                    );
                    grpc_pool.remove(&grpc_addr);
                    last_error = Some(e);
                    continue;
                } else if !should_retry {
                    error!("❌ [COMPUTER_FORWARD] 检测到不可重试错误，停止重试: {}", e);
                    last_error = Some(e);
                    break;
                }

                last_error = Some(e);
            }
        }
    }

    // 所有重试都失败
    if let Some(e) = last_error {
        error!(
            "❌ [COMPUTER_FORWARD] gRPC 最终调用失败: {}, user_id={}, project_id={}",
            e, request.user_id, project_id
        );

        // gRPC 通信失败，直接返回错误
        // 注：业务错误码（如 Agent busy）现在由 agent_runner 通过 grpc_response.error_code 返回
        HttpResult::error(
            shared_types::error_codes::ERR_GRPC_ERROR,
            &format!("容器通信失败: {}", e),
        )
    } else {
        HttpResult::error(shared_types::error_codes::ERR_UNKNOWN, "未知重试错误")
    }
}

/// 确保 project_id 对应的工作目录存在
///
/// Computer Agent Runner 的目录结构：
/// /app/computer-project-workspace/{user_id}/{project_id}/
///
/// 注意：这个目录已经在 docker-compose.yml 中挂载，可以直接在 rcoder 容器内创建
async fn ensure_project_workspace_exists(
    user_id: &str,
    project_id: &str,
    _container_ip: &str,
) -> Result<(), AppError> {
    // 项目工作目录路径
    let project_workspace_path = std::path::PathBuf::from("/app/computer-project-workspace")
        .join(user_id)
        .join(project_id);

    debug!(
        "📁 [COMPUTER_CHAT] 确保项目工作目录存在: {:?}",
        project_workspace_path
    );

    // 直接在 rcoder 容器内创建目录
    tokio::fs::create_dir_all(&project_workspace_path)
        .await
        .map_err(|e| {
            error!(
                "❌ [COMPUTER_CHAT] 创建项目工作目录失败: path={:?}, error={}",
                project_workspace_path, e
            );
            AppError::internal_server_error(&format!("创建项目工作目录失败: {}", e))
        })?;

    info!(
        "✅ [COMPUTER_CHAT] 项目工作目录已创建: user_id={}, project_id={}, path={:?}",
        user_id, project_id, project_workspace_path
    );

    Ok(())
}

/// 从 service_url 提取 gRPC 地址
fn extract_grpc_addr(service_url: &str, grpc_port: u16) -> Result<String, AppError> {
    let host = service_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .ok_or_else(|| AppError::internal_server_error("无效的 service_url"))?;

    Ok(format!("{}:{}", host, grpc_port))
}

// ============================================================================
// SSE 进度流处理器（复用现有的 agent_session_notification）
// ============================================================================

/// Computer Agent 进度通知 SSE 处理器
///
/// 复用现有的 agent_session_notification 处理器，
/// 因为 session_id 到容器的映射已经在 handle_computer_chat 中建立。
///
/// 客户端可以直接使用 `/agent/progress/{session_id}` 接口，
/// 或者使用 `/computer/progress/{session_id}` 作为别名。
pub use super::agent_session_notification::agent_session_notification as computer_session_notification;
