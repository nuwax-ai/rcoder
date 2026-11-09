//! Computer Agent Status Handler
//!
//! 处理 POST /computer/agent/status 请求

use axum::{Json, extract::State, http::HeaderMap};
use std::sync::Arc;
use tracing::{info, warn};

use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use shared_types::{
    ComputerAgentStatusRequest, ComputerAgentStatusResponse, HttpResult, I18nJsonOrQuery,
    error_codes::ERR_VALIDATION, get_i18n_message,
};

use super::locale_from_headers;

/// 查询 Computer Agent 状态
///
/// 直接使用 AGENT_REGISTRY 查询,无需 gRPC 调用
#[utoipa::path(
    post,
    path = "/computer/agent/status",
    request_body = ComputerAgentStatusRequest,
    responses(
        (status = 200, description = "Status query successful", body = HttpResult<ComputerAgentStatusResponse>),
        (status = 400, description = "Bad request - missing fields"),
        (status = 500, description = "Internal server error")
    ),
    tag = "Computer Agent"
)]
pub async fn handle_computer_status(
    State(_state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStatusRequest>,
) -> Result<Json<HttpResult<ComputerAgentStatusResponse>>, shared_types::AppError> {
    let locale = locale_from_headers(&headers);

    // 使用 garde 进行字段校验
    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    // garde 已校验 required, 此处仍做防御性处理: 生产路径不 panic, 返回 400。
    let Some(project_id) = request.project_id.as_ref() else {
        return Err(shared_types::AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.user_id_or_project_id_required", locale),
        ));
    };

    // 验证 user_id 或 project_id 至少有一个
    let user_id_empty = request.user_id.as_ref().is_none_or(|s| s.is_empty());
    if user_id_empty && project_id.is_empty() {
        return Err(shared_types::AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.user_id_or_project_id_required", locale),
        ));
    }

    info!(
        "🔍 [HTTP] Computer Agent 状态查询: user_id={:?}, project_id={}, pod_id={:?}, tenant_id={:?}, space_id={:?}, isolation_type={:?}",
        request.user_id,
        project_id,
        request.pod_id,
        request.tenant_id,
        request.space_id,
        request.isolation_type
    );

    // 从 AGENT_REGISTRY 查询 Agent 状态。在 guard 作用域内提取所需快照并
    // 立即释放 Ref,后续 /proc 资源扫描不得持有注册表读锁(锁纪律:
    // guard 不跨 I/O,避免阻塞注册表写方)。
    let agent_snapshot = AGENT_REGISTRY
        .get_agent_info(project_id)
        .filter(|info| {
            request
                .session_id
                .as_deref()
                .filter(|sid| !sid.is_empty())
                .is_none_or(|sid| {
                    shared_types::SessionHint::classify(Some(sid))
                        .as_session_id()
                        .is_some()
                        && info.session_id.0.as_ref() == sid
                })
        })
        .map(|info| {
            let session_id = shared_types::SessionHint::classify(Some(info.session_id.0.as_ref()))
                .as_session_id()
                .map(str::to_owned);
            let status = format!("{:?}", info.status);
            let (last_activity, created_at) = (info.last_activity, info.created_at);
            drop(info);
            (session_id, status, last_activity, created_at)
        });

    // agent 进程组资源采集(本地注册表直采;失败/未运行降级 None)。
    // 传入快照里的执行身份:registry 在两次读取之间换代时,资源缺省,
    // 状态(session_id)与资源保证来自同一执行。
    let (cpu_percent, memory_mb) = crate::service::agent_resource_monitor::sample_for_project(
        project_id,
        agent_snapshot
            .as_ref()
            .and_then(|(sid, _, _, _)| sid.as_deref()),
    );

    let response = match agent_snapshot {
        Some((session_id, status, last_activity, created_at)) => {
            // Agent 存在且活跃
            info!(
                "✅ [HTTP] Agent status: project_id={}, is_alive=true, session_id={:?}",
                project_id, session_id
            );

            ComputerAgentStatusResponse {
                user_id: request.user_id.clone(),
                project_id: project_id.to_string(),
                is_alive: true,
                session_id,
                status: Some(status),
                last_activity: Some(last_activity),
                created_at: Some(created_at),
                cpu_percent,
                memory_mb,
            }
        }
        None => {
            // Agent 不存在
            warn!("[HTTP] Agent not found: project_id={}", project_id);

            ComputerAgentStatusResponse {
                user_id: request.user_id.clone(),
                project_id: project_id.to_string(),
                is_alive: false,
                session_id: None,
                status: None,
                last_activity: None,
                created_at: None,
                cpu_percent: None,
                memory_mb: None,
            }
        }
    };

    Ok(Json(HttpResult::success(response)))
}
