//! userApp dev 状态查询变体（自 computer_agent_status_handler 拆出；原样搬迁）。

use super::retry::{GRPC_MAX_RETRIES, GetStatusParams, call_grpc_get_status_with_retry};
use super::*;

/// userApp dev 分派：查询 UserappBuilder 开发容器内 app 会话的 agent 状态。
///
/// 定位 = project 映射优先（builder 容器注册于 `state.projects[app_id]`）；
/// 映射 miss 时按 UserappBuilder 只读实时查（与 pod 族 status_userapp_dev
/// 同为只读不自愈）。无容器/not running/gRPC 失败均报 `not_alive`（幂等），
/// 不做 computer 路径的 self-healing——builder 注册由 ensure 链维护。
pub(super) async fn status_userapp_dev(
    state: &AppState,
    locale: &'static str,
    app_id: &str,
    request: &ComputerAgentStatusRequest,
) -> Result<HttpResult<ComputerAgentStatusResponse>, AppError> {
    let Some(container_info) = crate::handler::pod_handler::resolve_userapp_dev_container(
        state,
        app_id,
        "COMPUTER_AGENT_STATUS][USERAPP",
    )
    .await?
    else {
        return Ok(HttpResult::success(ComputerAgentStatusResponse::not_alive(
            request.user_id.clone(),
            app_id.to_string(),
        )));
    };

    if !crate::handler::pod_handler::is_container_running(&container_info.status) {
        info!(
            "⚠️ [COMPUTER_AGENT_STATUS][USERAPP] dev builder container not running: app_id={app_id}, status={}",
            container_info.status
        );
        return Ok(HttpResult::success(ComputerAgentStatusResponse::not_alive(
            request.user_id.clone(),
            app_id.to_string(),
        )));
    }

    let grpc_response = match call_grpc_get_status_with_retry(GetStatusParams {
        pool: &state.grpc_pool,
        container_name: &container_info.container_name,
        container_ip: &container_info.container_ip,
        namespace: &state.config.app_manager.namespace,
        project_id: app_id,
        max_retries: GRPC_MAX_RETRIES,
        locale,
        cluster_domain: &state.cluster_domain,
    })
    .await
    {
        Ok(response) => response,
        Err(e) => {
            warn!(
                "⚠️ [COMPUTER_AGENT_STATUS][USERAPP] gRPC GetStatus call failed: app_id={app_id}, error={e}"
            );
            return Ok(HttpResult::success(ComputerAgentStatusResponse::not_alive(
                request.user_id.clone(),
                app_id.to_string(),
            )));
        }
    };

    if !grpc_response.is_found {
        info!(
            "📭 [COMPUTER_AGENT_STATUS][USERAPP] agent not started: app_id={app_id}, is_found={}",
            grpc_response.is_found
        );
        return Ok(HttpResult::success(ComputerAgentStatusResponse::not_alive(
            request.user_id.clone(),
            app_id.to_string(),
        )));
    }

    // 会话补充信息从 project 记录读取（dev chat 响应后写入）；无记录只返回存活
    let (session_id, last_activity, created_at) = match state.get_project(app_id) {
        Some(p) => (
            p.session_id().map(|s| s.to_string()),
            Some(p.last_activity()),
            Some(p.created_at()),
        ),
        None => (None, None, None),
    };

    Ok(HttpResult::success(ComputerAgentStatusResponse {
        user_id: request.user_id.clone(),
        project_id: app_id.to_string(),
        is_alive: true,
        session_id,
        status: Some(grpc_response.status.clone()),
        last_activity,
        created_at,
    }))
}
