//! Computer Agent Status Handler
//!
//! 查询 Computer Agent 的运行状态（通过 gRPC GetStatus 主动确认）

use axum::extract::State;
use axum::http::HeaderMap;
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

use super::utils::{I18nJsonOrQuery, get_locale_from_headers};
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
    let project_id = match request.project_id.as_ref() {
        Some(pid) => pid,
        None => {
            tracing::error!("[COMPUTER_AGENT_STATUS] project_id is None after validation");
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
            ));
        }
    };

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

    // 用于日志输出的标识符（has_user_id / has_pod_id 已确保至少一个非空）
    let identifier_display = if has_user_id {
        format!("user_id={}", request.user_id.as_deref().unwrap_or(""))
    } else {
        format!("pod_id={}", request.pod_id.as_deref().unwrap_or(""))
    };

    info!(
        "🔍 [COMPUTER_AGENT_STATUS] Querying Agent status: {}, project_id={}",
        identifier_display, project_id
    );

    // 2. 查询容器信息（通过 Runtime）
    // 获取容器标识符（user_id 或 pod_id）
    let identifier = request.user_id.clone().or(request.pod_id.clone());
    let identifier_str = identifier.as_deref().unwrap_or("");

    // 获取容器信息（ComputerAgentRunner 使用 user_id 或 pod_id 作为容器标识）
    let container_info = match state
        .runtime()
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
    // 根据运行环境选择 gRPC 地址
    // - K8s 环境：使用 K8s Service FQDN（利用服务发现和负载均衡）
    // - Docker 环境：使用容器 IP（直接连接）
    let grpc_addr = if shared_types::is_kubernetes_runtime() {
        let svc_fqdn = super::utils::build_k8s_service_fqdn(
            &container_info.container_name,
            &state.config.app_manager.namespace,
            &state.cluster_domain,
        );
        let addr = format!("{}:{}", svc_fqdn, shared_types::GRPC_DEFAULT_PORT);
        debug!(
            "📡 [COMPUTER_AGENT_STATUS] Using K8s Service FQDN for gRPC: {}",
            addr
        );
        addr
    } else {
        let addr = format!("{}:{}", container_info.container_ip, shared_types::GRPC_DEFAULT_PORT);
        debug!(
            "📡 [COMPUTER_AGENT_STATUS] Using container IP for gRPC: {}",
            addr
        );
        addr
    };

    debug!(
        "📡 [COMPUTER_AGENT_STATUS] gRPC address: {}, project_id={}",
        grpc_addr, project_id
    );

    // 调用 gRPC GetStatus（带超时和重试，重试时自动重新获取 IP）
    let get_status_params = GetStatusParams {
        pool: &state.grpc_pool,
        container_name: &container_info.container_name,
        container_ip: &container_info.container_ip,
        namespace: &state.config.app_manager.namespace,
        project_id,
        max_retries: GRPC_MAX_RETRIES,
        locale,
        cluster_domain: &state.cluster_domain,
    };
    let grpc_response = match call_grpc_get_status_with_retry(get_status_params).await
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

    // 6. Agent 存活，从 存储 获取完整信息
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
        // no project record found，但 gRPC 确认 Agent 存在
        warn!(
            "⚠️ [COMPUTER_AGENT_STATUS] Agent exists but no project record (may be due to service restart causing state loss): {}, project_id={}. Attempting self-healing...",
            identifier_display, project_id
        );

        // 🛡️ 自愈逻辑 (Self-Healing) - M5 修复：加二次检查避免高频状态查询触发频繁写入
        //
        // 历史问题：状态查询接口（GET）在 project 记录缺失时主动 insert_project，
        // 破坏 CQRS（读路径不应写）。高频查询会持续争抢 storage 写锁。
        //
        // 修复策略：保留 self-healing（避免容器被孤立清理器误杀），但二次检查降低重复写：
        //   - 二次 contains_project 检查：若其他线程刚刚写过，跳过本次写
        //   - 即使触发写入也只写一次（insert_project 是 upsert 语义）
        //
        // 注意：contains_project + insert_project 之间仍有 race window（不是真 CAS），
        // 极端并发下可能两个请求都进入 insert 分支，但 upsert 语义保证最终一致，
        // 且 container_info 是实时查询的最新值，覆盖也不会丢失关键信息。
        if state.contains_project(project_id) {
            debug!(
                "[COMPUTER_AGENT_STATUS] Self-healing skipped: project record concurrently inserted by another request: project_id={}",
                project_id
            );
        } else {
            let mut project_info =
                shared_types::ProjectAndContainerInfo::new(project_id.to_string());
            project_info.set_user_id(request.user_id.clone());
            project_info.set_pod_id(request.pod_id.clone());

            // 恢复容器信息
            project_info.set_container(Some(container_info.clone()));
            project_info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));

            // insert into storage（upsert 语义，安全）
            state
                .insert_project(project_id.to_string(), Arc::new(project_info.clone()))
                .map_err(|e| {
                    tracing::error!("[STORAGE] insert_project failed: {}", e);
                    e
                })?;

            info!(
                "🔄 [COMPUTER_AGENT_STATUS] ✅ Self-healing succeeded: restored project record project_id={}, {}",
                project_id, identifier_display
            );
        }

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
/// - `runtime`: 容器运行时
/// - `container_name`: 容器名称
/// - `fallback_ip`: 回退 IP 地址
/// - `rcoder_prefix`: RCoder 容器前缀
/// - `computer_prefix`: Computer 容器前缀
/// - `namespace`: K8s namespace
/// - `project_id`: 项目 ID
/// - `max_retries`: 最大重试次数
/// - `locale`: 语言设置
///
/// # 返回
/// - `Ok(status)`: 从 Agent 返回的状态字符串（可能的值取决于 Agent 实现，通常为 "idle", "busy", "error", "not_found" 等）
/// - `Err(e)`: gRPC 调用失败（网络错误、超时、连接失败等）
///
/// # 重试策略
/// - 仅对可重试的错误进行重试：Unavailable, DeadlineExceeded, Unknown, Internal
///
/// gRPC GetStatus 请求参数
///
/// 封装了调用 gRPC GetStatus 所需的所有参数，
/// 避免函数参数过多。
struct GetStatusParams<'a> {
    /// gRPC 连接池
    pool: &'a Arc<crate::grpc::GrpcChannelPool>,
    /// 容器名称
    container_name: &'a str,
    /// 容器 IP（Docker 环境使用）
    container_ip: &'a str,
    /// K8s namespace
    namespace: &'a str,
    /// 项目 ID
    project_id: &'a str,
    /// 最大重试次数
    max_retries: u32,
    /// 语言设置
    locale: &'static str,
    /// K8s 集群域名
    cluster_domain: &'a str,
}

/// - 使用指数退避：100ms, 200ms, 400ms
/// - 失败后自动从连接池移除失败的连接，并重新获取容器 IP
async fn call_grpc_get_status_with_retry(
    params: GetStatusParams<'_>,
) -> anyhow::Result<shared_types::grpc::GetStatusResponse> {
    let mut last_error = None;

    // 根据运行环境选择 gRPC 地址
    // - K8s 环境：使用 K8s Service FQDN（利用服务发现和负载均衡）
    // - Docker 环境：使用容器 IP（直接连接）
    let grpc_addr = if shared_types::is_kubernetes_runtime() {
        let svc_fqdn = super::utils::build_k8s_service_fqdn(
            params.container_name,
            params.namespace,
            params.cluster_domain,
        );
        let addr = format!("{}:{}", svc_fqdn, shared_types::GRPC_DEFAULT_PORT);
        debug!(
            "📡 [GRPC_GET_STATUS] Using K8s Service FQDN for gRPC: {}",
            addr
        );
        addr
    } else {
        let addr = format!("{}:{}", params.container_ip, shared_types::GRPC_DEFAULT_PORT);
        debug!(
            "📡 [GRPC_GET_STATUS] Using container IP for gRPC: {}",
            addr
        );
        addr
    };

    for attempt in 1..=params.max_retries {
        // K8s Service FQDN 是稳定的，不需要重新解析
        // 直接使用原来的 FQDN 进行重试
        if attempt > 1 {
            debug!(
                "🔄 [GRPC_GET_STATUS] Retrying with same K8s Service FQDN: {}",
                grpc_addr
            );
        }

        match params.pool.get_client(&grpc_addr).await {
            Ok(mut client) => {
                let request = shared_types::grpc::GetStatusRequest {
                    project_id: params.project_id.to_string(),
                    session_id: String::new(), // 查询项目级别状态
                };

                // 设置超时
                let mut tonic_request = crate::grpc::new_request_with_locale(request, params.locale);
                tonic_request
                    .set_timeout(std::time::Duration::from_secs(GRPC_REQUEST_TIMEOUT_SECS));

                match client.get_status(tonic_request).await {
                    Ok(response) => {
                        let grpc_response = response.into_inner();
                        debug!(
                            "✅ [GRPC_GET_STATUS] Attempt {} succeeded: project_id={}, status={}, is_found={}",
                            attempt, params.project_id, grpc_response.status, grpc_response.is_found
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

                        if should_retry && attempt < params.max_retries {
                            warn!(
                                "⚠️ [GRPC_GET_STATUS] Attempt {} failed (retryable): project_id={}, code={:?}, error={}",
                                attempt,
                                params.project_id,
                                e.code(),
                                e
                            );
                            // 从连接池移除失败的连接
                            params.pool.remove(&grpc_addr).await;
                            last_error = Some(anyhow::anyhow!("gRPC call failed: {}", e));

                            // 指数退避: 100ms, 200ms, 400ms
                            let delay_ms = 100 * (1 << (attempt - 1));
                            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                            continue;
                        } else {
                            error!(
                                "❌ [GRPC_GET_STATUS] Attempt {} failed (non-retryable or max retries reached): project_id={}, code={:?}, error={}",
                                attempt,
                                params.project_id,
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
                params.pool.remove(&grpc_addr).await;
                last_error = Some(e);
                if attempt < params.max_retries {
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
