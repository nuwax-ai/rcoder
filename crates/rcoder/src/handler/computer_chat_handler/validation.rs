//! Computer Chat 请求校验与准备阶段
//!
//! 从 `handle_computer_chat_internal` 抽出：user_id 校验、隔离参数校验、
//! project_id / work_dir_id 解析与校验、资源限制校验。

use shared_types::{ComputerChatRequest, IsolationType};
use std::str::FromStr;
use tracing::{error, info};

use crate::HttpResult;

use super::super::chat_forward::ChatFlowExit;

/// 校验请求并解析出 project_id / work_dir_id
///
/// 阶段内容（与原内联实现逐步一致）：
/// 1. 验证 user_id
/// 2. 隔离类型参数校验（pod_id 存在时 isolation_type/tenant_id/space_id 必须非空）
/// 3. 生成或使用提供的 project_id
/// 4. 解析并校验 work_dir_id
/// 5. 验证资源限制配置
#[allow(clippy::result_large_err)]
pub(super) fn validate_and_prepare_request(
    request: &mut ComputerChatRequest,
    locale: &'static str,
) -> Result<(String, String), ChatFlowExit> {
    // 1. 验证 user_id
    if request.user_id.trim().is_empty() {
        error!("[COMPUTER_CHAT] user_id is required");
        return Err(ChatFlowExit::response(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        )));
    }

    // ========== 隔离类型参数校验 ==========
    // IF pod_id IS NOT NULL THEN isolation_type, tenant_id, space_id 必须非空
    validate_isolation_params(request, locale)?;

    // 2. 生成或使用提供的 project_id
    let project_id = match &request.project_id {
        Some(id) if !id.trim().is_empty() => id.clone(),
        _ => {
            let generated_id = crate::service::container_manager::generate_project_id();
            request.project_id = Some(generated_id.clone());
            generated_id
        }
    };
    // 客户端提供的 project_id 直接进容器命名/存储 key/gRPC 请求——畸形值
    //（超长/特殊字符）会 500 或污染存储键，源头校验（与 userapp 分支同款）
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

    info!(
        "🚀 [COMPUTER_CHAT] Starting to process request: user_id={}, project_id={}, session_id={:?}, prompt_len={}, attachments={}, model_provider={:?}, agent_config={:?}",
        request.user_id,
        project_id,
        request.session_id,
        request.prompt.len(),
        request.attachments.len(),
        request.model_provider,
        request.agent_config
    );

    // 3. 验证资源限制配置
    if let Some(ref agent_config) = request.agent_config
        && let Some(ref resource_limits) = agent_config.resource_limits
        && let Err(e) = resource_limits.validate()
    {
        error!("[COMPUTER_CHAT] Resource limits validation failed: {}", e);
        return Err(ChatFlowExit::response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
            locale,
            &format!("Resource limits invalid: {}", e),
        )));
    }

    Ok((project_id, work_dir_id))
}

/// 隔离类型参数校验：pod_id 存在时 isolation_type/tenant_id/space_id 必须非空，
/// 且 isolation_type 值有效（大小写不敏感）
#[allow(clippy::result_large_err)]
fn validate_isolation_params(
    request: &ComputerChatRequest,
    locale: &'static str,
) -> Result<(), ChatFlowExit> {
    if request.pod_id.is_none() {
        return Ok(());
    }

    if request.isolation_type.is_none() {
        error!(
            "[COMPUTER_CHAT] Validation failed: isolation_type is required when pod_id is provided"
        );
        return Err(ChatFlowExit::response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "isolation_type is required when pod_id is provided",
        )));
    }
    if request.tenant_id.is_none() {
        error!("[COMPUTER_CHAT] Validation failed: tenant_id is required when pod_id is provided");
        return Err(ChatFlowExit::response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "tenant_id is required when pod_id is provided",
        )));
    }
    if request.space_id.is_none() {
        error!("[COMPUTER_CHAT] Validation failed: space_id is required when pod_id is provided");
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
            "[COMPUTER_CHAT] Validation failed: invalid isolation_type '{}', expected tenant|space|project",
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
            "🔒 [COMPUTER_CHAT] Isolation parameters validated: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
            pid, it, tid, sid
        );
    }

    Ok(())
}
