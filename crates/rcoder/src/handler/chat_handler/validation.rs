//! chat 请求校验与路由解析（从 chat_handler.rs 拆出）。

use anyhow::Result;
use shared_types::{AgentChatRequest, IsolationType};
use std::str::FromStr;
use tracing::{error, info};

use crate::handler::chat_forward::ChatFlowExit;
use crate::handler::utils::build_workspace_path;
use crate::{AppError, HttpResult, service};

use super::types::ChatRouteTarget;

/// 第一段：请求校验与路由解析
///
/// 包含：project_id 生成、work_dir_id 解析与校验、隔离类型参数校验、
/// 工作空间路径构建、资源限制校验。
#[allow(clippy::result_large_err)]
pub(super) fn validate_and_route_chat_request(
    request: &mut AgentChatRequest,
    locale: &'static str,
) -> Result<ChatRouteTarget, ChatFlowExit> {
    let project_id = match &request.project_id {
        Some(id) => id.clone(),
        None => {
            let project_id = service::container_manager::generate_project_id();
            request.project_id = Some(project_id.clone()); // 设置 project_id
            project_id
        }
    };
    // 客户端提供的 project_id 直接进容器命名/存储 key/gRPC 请求——畸形值
    //（超长/特殊字符）会 500 或污染存储键，源头校验（自动生成值恒合法跳过）
    if let Err(e) = shared_types::validate_identifier(&project_id, "project_id") {
        return Err(ChatFlowExit::response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            &e,
        )));
    }

    // 确定用于拼接工作目录的标识符
    // agent_work_dir 用于替代 project_id 参与工作目录路径拼接
    let work_dir_id = request
        .agent_work_dir
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| project_id.clone());

    // 校验 work_dir_id（无论来源，用于路径拼接的标识符都应校验）
    if let Err(e) = shared_types::validate_identifier(&work_dir_id, "agent_work_dir") {
        return Err(ChatFlowExit::response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            &e,
        )));
    }

    // ========== 隔离类型参数校验 ==========
    // IF pod_id IS NOT NULL THEN isolation_type, tenant_id, space_id 必须非空
    validate_chat_isolation_params(request, locale)?;

    // ========== 构建工作空间路径 ==========
    // 根据 isolation_type 确定容器内工作目录：
    // - tenant/space: /app/project_workspace/{tenant_id}/{space_id}/{work_dir_id}
    // - project 或默认: /app/project_workspace/{work_dir_id}
    let container_work_path = build_workspace_path(
        request.isolation_type.as_deref(),
        request.tenant_id.as_deref(),
        request.space_id.as_deref(),
        &work_dir_id,
    )
    .map_err(|e| AppError::validation_error(&e.to_string()))?;

    info!(
        "📁 [CHAT] Workspace path determined: {} (isolation_type={})",
        container_work_path,
        request.isolation_type.as_deref().unwrap_or("project")
    );

    // 验证资源限制配置
    if let Some(ref agent_config) = request.agent_config
        && let Some(ref resource_limits) = agent_config.resource_limits
    {
        resource_limits
            .validate()
            .map_err(|e| AppError::validation_error(&format!("Invalid resource limits: {}", e)))?;
    }

    info!(
        "🚀 [CHAT] Starting to process chat request: project_id={}, session_id={:?}, prompt_length={}, attachments_count={}, model_provider={}",
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
    info!(
        "🔧 [CHAT] agent_config: project_id={}, agent_config={:?}",
        project_id, request.agent_config
    );

    Ok(ChatRouteTarget {
        project_id,
        work_dir_id,
        container_work_path,
    })
}

/// 隔离类型参数校验：pod_id 存在时 isolation_type/tenant_id/space_id 必须非空，
/// 且 isolation_type 值有效（大小写不敏感）
#[allow(clippy::result_large_err)]
pub(super) fn validate_chat_isolation_params(
    request: &AgentChatRequest,
    locale: &'static str,
) -> Result<(), ChatFlowExit> {
    if request.pod_id.is_none() {
        return Ok(());
    }

    if request.isolation_type.is_none() {
        error!("[CHAT] Validation failed: isolation_type is required when pod_id is provided");
        return Err(ChatFlowExit::response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "isolation_type is required when pod_id is provided",
        )));
    }
    if request.tenant_id.is_none() {
        error!("[CHAT] Validation failed: tenant_id is required when pod_id is provided");
        return Err(ChatFlowExit::response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "tenant_id is required when pod_id is provided",
        )));
    }
    if request.space_id.is_none() {
        error!("[CHAT] Validation failed: space_id is required when pod_id is provided");
        return Err(ChatFlowExit::response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "space_id is required when pod_id is provided",
        )));
    }

    // 验证 isolation_type 值有效（大小写不敏感）
    if let Some(ref it) = request.isolation_type
        && IsolationType::from_str(it).is_err()
    {
        error!(
            "[CHAT] Validation failed: invalid isolation_type '{}', expected tenant|space|project",
            it
        );
        return Err(ChatFlowExit::response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            &format!(
                "invalid isolation_type '{}', expected: tenant, space, project",
                it
            ),
        )));
    }

    // 记录验证通过的参数（此时 pod_id, isolation_type, tenant_id, space_id 必定为 Some）
    if let (Some(pid), Some(it), Some(tid), Some(sid)) = (
        request.pod_id.as_deref(),
        request.isolation_type.as_deref(),
        request.tenant_id.as_deref(),
        request.space_id.as_deref(),
    ) {
        info!(
            "🔒 [CHAT] Isolation parameters validated: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
            pid, it, tid, sid
        );
    }

    Ok(())
}
