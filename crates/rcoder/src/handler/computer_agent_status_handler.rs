//! Computer Agent Status Handler
//!
//! 查询 Computer Agent 的运行状态（通过 gRPC GetStatus 主动确认）

use axum::extract::State;
use axum::http::HeaderMap;
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

use super::utils::{I18nJsonOrQuery, get_locale_from_headers, get_realtime_container_ip};
use crate::router::AppState;
use crate::{AppError, HttpResult};
use shared_types::{ComputerAgentStatusRequest, ComputerAgentStatusResponse};

/// gRPC GetStatus 最大重试次数
const GRPC_MAX_RETRIES: u32 = 3;

/// gRPC GetStatus 请求超时时间（秒）
const GRPC_REQUEST_TIMEOUT_SECS: u64 = 5;

/// 处理 Computer Agent 状态查询
///
/// 核心流程：
/// 1. 验证 user_id 和 project_id
/// 2. 查询容器是否存在且运行中
/// 3. 主动调用 gRPC GetStatus 确认 Agent 真实状态
/// 4. 返回综合状态信息
#[utoipa::path(
    post,
    path = "/computer/agent/status",
    request_body(
        content = ComputerAgentStatusRequest,
        description = "Computer Agent 状态查询请求",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功获取 Agent 状态",
            body = HttpResult<ComputerAgentStatusResponse>,
            examples(
                ("Agent 已启动" = (value = json!({
                    "success": true,
                    "code": "0000",
                    "message": "Success",
                    "data": {
                        "user_id": "user_123",
                        "project_id": "proj_456",
                        "is_alive": true,
                        "session_id": "session_abc123",
                        "status": "idle",
                        "last_activity": "2024-01-01T12:00:00Z",
                        "created_at": "2024-01-01T10:00:00Z"
                    }
                }))),
                ("Agent 未启动" = (value = json!({
                    "success": true,
                    "code": "0000",
                    "message": "Success",
                    "data": {
                        "user_id": "user_123",
                        "project_id": "proj_456",
                        "is_alive": false
                    }
                })))
            )
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "computer",
    operation_id = "computer_agent_status",
    summary = "查询 Computer Agent 状态",
    description = "查询指定 user_id + project_id 对应的 Computer Agent 是否已启动。通过主动调用子容器的 gRPC GetStatus 接口确认 Agent 真实状态。"
)]
#[instrument(skip(state))]
pub async fn computer_agent_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStatusRequest>,
) -> Result<HttpResult<ComputerAgentStatusResponse>, AppError> {
    // 获取语言设置
    let locale = get_locale_from_headers(&headers);

    // 使用 garde 进行字段校验
    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    let project_id = request
        .project_id
        .as_ref()
        .expect("validated: project_id is required and non-empty");

    // 1. 参数验证：user_id 或 pod_id 至少有一个
    let has_user_id = request
        .user_id
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_pod_id = request
        .pod_id
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_user_id && !has_pod_id {
        error!("[COMPUTER_AGENT_STATUS] user_id or pod_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    // 用于日志输出的标识符
    let identifier_display = if has_user_id {
        format!("user_id={}", request.user_id.as_ref().unwrap())
    } else {
        format!("pod_id={}", request.pod_id.as_ref().unwrap())
    };

    info!(
        "🔍 [COMPUTER_AGENT_STATUS] Querying Agent status: {}, project_id={}",
        identifier_display, project_id
    );

    // 2. 查询容器信息（通过 Runtime）
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| {
            error!("[COMPUTER_AGENT_STATUS] Failed to get runtime: {}", e);
            AppError::internal_server_error(&format!("Failed to get runtime: {}", e))
        })?;

    // 获取容器标识符（user_id 或 pod_id）
    let identifier = request.user_id.clone().or(request.pod_id.clone());
    let identifier_str = identifier.as_deref().unwrap_or("");

    // 获取容器信息（ComputerAgentRunner 使用 user_id 或 pod_id 作为容器标识）
    let container_info = match runtime
        .get_container_info_by_identifier(
            identifier_str,
            &shared_types::ServiceType::ComputerAgentRunner,
        )
        .await
    {
        Ok(Some(info)) => info,
        Ok(None) => {
            info!(
                "📭 [COMPUTER_AGENT_STATUS] Container not found: identifier={}",
                identifier_str
            );
            // Early return: 直接 move request 的字段
            return Ok(HttpResult::success(ComputerAgentStatusResponse::not_alive(
                request.user_id.clone(),
                project_id.to_string(),
            )));
        }
        Err(e) => {
            error!(
                "❌ [COMPUTER_AGENT_STATUS] Failed to query container info: identifier={}, error={}",
                identifier_str, e
            );
            return Err(AppError::internal_server_error(&format!(
                "Failed to query container info: {}",
                e
            )));
        }
    };

    // 3. 检查容器是否运行中
    if container_info.status != "running" {
        info!(
            "⚠️ [COMPUTER_AGENT_STATUS] Container not running: identifier={}, status={}",
            identifier_str, container_info.status
        );
        // Early return: 直接 move request 的字段
        return Ok(HttpResult::success(ComputerAgentStatusResponse::not_alive(
            request.user_id.clone(),
            project_id.to_string(),
        )));
    }

    info!(
        "✅ [COMPUTER_AGENT_STATUS] Container running: container_id={}, container_ip={}",
        container_info.container_id, container_info.container_ip
    );

    // 4. 主动调用 gRPC GetStatus 确认 Agent 真实状态
    // 使用实时 IP 获取，避免 restart 后 IP 过期
    let grpc_addr = match get_realtime_container_ip(
        &container_info.container_name,
        &container_info.container_ip,
        &state.container_prefix_rcoder,
        &state.container_prefix_computer,
    )
    .await
    {
        Ok(ip) => format!("{}:{}", ip, shared_types::GRPC_DEFAULT_PORT),
        Err(e) => {
            error!(
                "❌ [COMPUTER_AGENT_STATUS] Failed to get container IP: identifier={}, error={}",
                identifier_str, e
            );
            // Early return: 直接 move request 的字段
            return Ok(HttpResult::success(ComputerAgentStatusResponse::not_alive(
                request.user_id.clone(),
                project_id.to_string(),
            )));
        }
    };

    debug!(
        "📡 [COMPUTER_AGENT_STATUS] gRPC address: {}, project_id={}",
        grpc_addr, project_id
    );

    // 调用 gRPC GetStatus（带超时和重试，重试时自动重新获取 IP）
    let grpc_response = match call_grpc_get_status_with_retry(
        &state.grpc_pool,
        &container_info.container_name,
        &container_info.container_ip,
        &state.container_prefix_rcoder,
        &state.container_prefix_computer,
        project_id,
        GRPC_MAX_RETRIES,
        locale,
    )
    .await
    {
        Ok(response) => response,
        Err(e) => {
            warn!(
                "⚠️ [COMPUTER_AGENT_STATUS] gRPC GetStatus call failed: {}, project_id={}, error={}",
                identifier_display, project_id, e
            );
            // gRPC 调用失败视为 Agent 不存在
            // Early return: 直接 move request 的字段
            return Ok(HttpResult::success(ComputerAgentStatusResponse::not_alive(
                request.user_id.clone(),
                project_id.to_string(),
            )));
        }
    };

    // 5. 使用 is_found 字段判断 Agent 是否存活
    let is_alive = grpc_response.is_found;

    if !is_alive {
        info!(
            "📭 [COMPUTER_AGENT_STATUS] Agent not started: {}, project_id={}, is_found={}",
            identifier_display, project_id, grpc_response.is_found
        );
        return Ok(HttpResult::success(ComputerAgentStatusResponse::not_alive(
            request.user_id.clone(),
            project_id.to_string(),
        )));
    }

    // 6. Agent 存活，从 DuckDB 获取完整信息
    let response = if let Some(project_info) = state.get_project(project_id) {
        ComputerAgentStatusResponse {
            user_id: request.user_id.clone(),
            project_id: project_id.to_string(),
            is_alive: true,
            session_id: project_info.session_id().map(|s| s.to_string()),
            status: Some(grpc_response.status.clone()),
            last_activity: Some(project_info.last_activity()),
            created_at: Some(project_info.created_at()),
        }
    } else {
        // DuckDB 中无记录，但 gRPC 确认 Agent 存在
        warn!(
            "⚠️ [COMPUTER_AGENT_STATUS] Agent exists but no DuckDB record (may be due to service restart causing state loss): {}, project_id={}. Attempting self-healing...",
            identifier_display, project_id
        );

        // 🛡️ 自愈逻辑 (Self-Healing)
        // 自动恢复丢失的项目记录，防止容器被孤立清理器误杀
        let mut project_info = shared_types::ProjectAndContainerInfo::new(project_id.to_string());
        project_info.set_user_id(request.user_id.clone());
        project_info.set_pod_id(request.pod_id.clone());

        // 恢复容器信息
        // 注意：这里我们使用查询到的 container_info
        project_info.set_container(Some(container_info.clone()));
        project_info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));

        // 设置状态 (根据 gRPC 返回的状态)
        // gRPC status: "idle", "busy" -> 对应 AgentStatus
        // 简单起见，我们先恢复记录，状态会在后续的心跳或交互中更新

        // 如果 gRPC 返回了 session_id（虽然 GetStatus 通常不返回 session_id），尝试恢复
        // 但目前的 GetStatusResponse 没有 session_id 字段，所以只能置空或尝试从其他地方恢复
        // 这里暂时置空，等下次聊天时会自动更新

        // 插入到 DuckDB
        state.insert_project(project_id.to_string(), Arc::new(project_info.clone()));

        info!(
            "🔄 [COMPUTER_AGENT_STATUS] ✅ Self-healing succeeded: restored project record project_id={}, {}",
            project_id, identifier_display
        );

        ComputerAgentStatusResponse {
            user_id: request.user_id.clone(),
            project_id: project_id.to_string(),
            is_alive: true,
            session_id: None, // 恢复时暂时无法获知 session_id
            status: Some(grpc_response.status.clone()),
            // 使用当前时间作为最后活动时间，避免立即被清理
            last_activity: Some(chrono::Utc::now()),
            created_at: Some(chrono::Utc::now()), // 使用当前时间作为创建时间（近似）
        }
    };

    info!(
        "✅ [COMPUTER_AGENT_STATUS] Agent status query completed: {}, project_id={}, is_alive={}, status={}",
        identifier_display,
        project_id,
        response.is_alive,
        response.status.as_deref().unwrap_or("unknown")
    );

    Ok(HttpResult::success(response))
}

/// 调用 gRPC GetStatus（带重试机制）
///
/// # 参数
/// - `pool`: gRPC 连接池
/// - `grpc_addr`: gRPC 服务地址
/// - `project_id`: 项目 ID
/// - `max_retries`: 最大重试次数
///
/// # 返回
/// - `Ok(status)`: 从 Agent 返回的状态字符串（可能的值取决于 Agent 实现，通常为 "idle", "busy", "error", "not_found" 等）
/// - `Err(e)`: gRPC 调用失败（网络错误、超时、连接失败等）
///
/// # 重试策略
/// - 仅对可重试的错误进行重试：Unavailable, DeadlineExceeded, Unknown, Internal
/// - 使用指数退避：100ms, 200ms, 400ms
/// - 失败后自动从连接池移除失败的连接，并重新获取容器 IP
#[allow(clippy::too_many_arguments)]
async fn call_grpc_get_status_with_retry(
    pool: &Arc<crate::grpc::GrpcChannelPool>,
    container_name: &str,
    fallback_ip: &str,
    rcoder_prefix: &str,
    computer_prefix: &str,
    project_id: &str,
    max_retries: u32,
    locale: &'static str,
) -> anyhow::Result<shared_types::grpc::GetStatusResponse> {
    let mut last_error = None;
    let mut grpc_addr = format!("{}:{}", fallback_ip, shared_types::GRPC_DEFAULT_PORT);

    for attempt in 1..=max_retries {
        // 重新获取最新容器 IP（每次重试时）
        if attempt > 1 {
            match get_realtime_container_ip(
                container_name,
                fallback_ip,
                rcoder_prefix,
                computer_prefix,
            )
            .await
            {
                Ok(ip) => {
                    let new_addr = format!("{}:{}", ip, shared_types::GRPC_DEFAULT_PORT);
                    info!(
                        "🔄 [GRPC_GET_STATUS] Container IP re-resolved: {} -> {}",
                        grpc_addr, new_addr
                    );
                    grpc_addr = new_addr;
                }
                Err(e) => {
                    warn!(
                        "⚠️ [GRPC_GET_STATUS] Failed to re-resolve container IP, keeping old address: {}",
                        e
                    );
                }
            }
        }

        match pool.get_client(&grpc_addr).await {
            Ok(mut client) => {
                let request = shared_types::grpc::GetStatusRequest {
                    project_id: project_id.to_string(),
                    session_id: String::new(), // 查询项目级别状态
                };

                // 设置超时
                let mut tonic_request = crate::grpc::new_request_with_locale(request, locale);
                tonic_request
                    .set_timeout(std::time::Duration::from_secs(GRPC_REQUEST_TIMEOUT_SECS));

                match client.get_status(tonic_request).await {
                    Ok(response) => {
                        let grpc_response = response.into_inner();
                        debug!(
                            "✅ [GRPC_GET_STATUS] Attempt {} succeeded: project_id={}, status={}, is_found={}",
                            attempt, project_id, grpc_response.status, grpc_response.is_found
                        );
                        return Ok(grpc_response);
                    }
                    Err(e) => {
                        // 直接判断原始 tonic::Status，避免信息丢失
                        let should_retry = matches!(
                            e.code(),
                            tonic::Code::Unavailable
                                | tonic::Code::DeadlineExceeded
                                | tonic::Code::Unknown
                                | tonic::Code::Internal
                        );

                        if should_retry && attempt < max_retries {
                            warn!(
                                "⚠️ [GRPC_GET_STATUS] Attempt {} failed (retryable): project_id={}, code={:?}, error={}",
                                attempt,
                                project_id,
                                e.code(),
                                e
                            );
                            // 从连接池移除失败的连接
                            pool.remove(&grpc_addr);
                            last_error = Some(anyhow::anyhow!("gRPC call failed: {}", e));

                            // 指数退避: 100ms, 200ms, 400ms
                            let delay_ms = 100 * (1 << (attempt - 1));
                            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                            continue;
                        } else {
                            error!(
                                "❌ [GRPC_GET_STATUS] Attempt {} failed (non-retryable or max retries reached): project_id={}, code={:?}, error={}",
                                attempt,
                                project_id,
                                e.code(),
                                e
                            );
                            return Err(anyhow::anyhow!("gRPC call failed: {}", e));
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    "⚠️ [GRPC_GET_STATUS] Attempt {} to get gRPC client failed: error={}",
                    attempt, e
                );
                // 从连接池移除可能失效的连接
                pool.remove(&grpc_addr);
                last_error = Some(e);
                if attempt < max_retries {
                    // 指数退避: 100ms, 200ms, 400ms
                    let delay_ms = 100 * (1 << (attempt - 1));
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                    continue;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Unknown error")))
}
