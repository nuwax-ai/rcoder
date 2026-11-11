//! Computer Agent 重启处理器
//!
//! 处理重启特定 project_id 的 Agent 请求（不销毁容器）。典型场景：发现
//! agent 占用内存等资源过高时人工重启释放资源（配合 `/computer/agent/status`
//! 的 cpu_percent/memory_mb 观察）。
//!
//! 与 [`crate::handler::computer_agent_stop`] 同构，仅三处行为差异（勿"对齐"
//! 回 stop 语义）：
//! 1. **保留会话注册**：跳过 `capture_project_stop_registration` 捕获与
//!    `clear_sessions_if_current_durable`/`clear_userapp_agent_sessions` 清理
//!    ——注册里的 session_id 保留，同一 project 的下次 `/computer/chat` 经
//!    `resolve_forward_request` 注入该 ID，agent_runner spawn 新进程后由 SACP
//!    `session/load` 恢复上下文（失败自动降级新会话）。
//! 2. **幂等成功**：`not_found`/`already_stopped` 视为重启成功（目标态 =
//!    agent 进程不在，已达成即成功）。
//! 3. **响应带 `resumed_session_id`**：告知前端重启后上下文是否可恢复。

use axum::extract::State;
use axum::http::HeaderMap;
use std::sync::Arc;
use tracing::instrument;

use crate::{AppError, HttpResult, router::AppState};
use shared_types::{ComputerAgentRestartResponse, ComputerAgentStopRequest};

use super::utils::{I18nJsonOrQuery, extract_grpc_addr, get_locale_from_headers};

/// 本文件专用日志宏。
///
/// 为什么不用裸 `info!/warn!/error!`：默认 `EnvFilter` 指令是
/// `rcoder=debug,tower_http=debug,axum=info,hyper=info,tonic=info`
/// （见 `rcoder-telemetry::subscriber`），而它的匹配方式是**目标字符串前缀**
/// （tracing-subscriber `env/directive.rs`: `meta.target().starts_with(target)`）。
/// `http_server` 这个 target 不以 `rcoder` 开头、也没有对应指令，于是整个
/// http-server crate 的日志都是 OFF —— 本接口的日志一条都进不了日志文件。
///
/// 这里显式用 `rcoder::computer_agent_restart` 作为 target，让**本接口**的日志
/// 可见，同时不动全局 filter，避免改变其他 handler 的日志行为。
macro_rules! restart_log {
    ($level:ident, $($arg:tt)*) => {
        tracing::$level!(target: "rcoder::computer_agent_restart", $($arg)*)
    };
}

/// 重启 Computer Agent
///
/// 重启特定 user_id 下的特定 project_id 的 Agent：优雅停止当前 agent 执行并
/// 确认进程退出，容器继续运行；会话注册保留，下次对话自动尝试恢复上下文。
///
/// agent 未在运行时返回成功（幂等——目标态已达成，无需动作）。
#[utoipa::path(
    post,
    path = "/computer/agent/restart",
    request_body(
        content = ComputerAgentStopRequest,
        description = "重启特定 project_id 的 Agent 请求（与 stop 请求同形）",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功重启 Agent（含幂等成功：agent 未运行时无需重启）",
            body = HttpResult<ComputerAgentRestartResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "message": "Agent 已重启，下次对话将尝试恢复上下文",
                    "user_id": "user_123",
                    "project_id": "proj_456",
                    "resumed_session_id": "session789"
                },
                "error": null
            })
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
            status = 404,
            description = "找不到指定的容器或 Agent（定位与执行身份不符）",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "computer",
    operation_id = "computer_agent_restart",
    summary = "重启 Computer Agent",
    description = "优雅停止并确认退出特定 project_id 的 Agent 执行，不销毁容器；与 stop 的差异是保留会话注册，同一 project 的下次对话自动尝试通过 session/load 恢复上下文（失败降级新会话）。agent 未运行时幂等成功。UserApp 保留 project_id 的独立 agent 身份；显式 service_type=userapp + app_id 或现有 user_id/project_id/session_id 请求的持久绑定均可定位共享 UserappBuilder，agent 会话仅 dev 阶段。"
)]
#[instrument(skip(state))]
pub async fn computer_agent_restart(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStopRequest>,
) -> Result<HttpResult<ComputerAgentRestartResponse>, AppError> {
    // 获取语言设置
    let locale = get_locale_from_headers(&headers);

    // 0. userApp 分派（service_type=userapp + app_id；agent 会话
    //    仅存在于 dev 的 UserappBuilder 开发容器）
    match super::pod_handler::parse_app_target(
        request.app_id.as_deref(),
        request.app_stage.as_deref(),
        request.service_type.as_deref(),
    ) {
        Ok(super::pod_handler::AppTarget::NotApp) => {}
        Ok(super::pod_handler::AppTarget::Dev(app_id)) => {
            restart_log!(
                info,
                "🔁 [COMPUTER_RESTART] userApp dev dispatch: app_id={}, session_id={:?}",
                app_id,
                request.session_id
            );
            let target = super::userapp_agent_target::resolve_agent_target(
                &state,
                request.user_id.as_deref(),
                request.project_id.as_deref(),
                request.session_id.as_deref(),
                Some(&app_id),
            )
            .await?;
            return restart_userapp_dev(&state, locale, target, &request).await;
        }
        Ok(super::pod_handler::AppTarget::Prod(_)) => {
            return Ok(super::pod_handler::agent_prod_unsupported(locale));
        }
        Err(e) => return Ok(super::pod_handler::invalid_app_target_response(locale, &e)),
    }

    // 使用 garde 进行字段校验
    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    let project_id = request
        .project_id
        .as_ref()
        .ok_or_else(|| AppError::validation_error("project_id is required and non-empty"))?;

    // 1. 验证参数：user_id 或 pod_id 至少有一个
    let has_user_id = request
        .user_id
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    let has_pod_id = request
        .pod_id
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    if !has_user_id && !has_pod_id {
        restart_log!(error, "[COMPUTER_RESTART] user_id or pod_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    if let Some(target) = super::userapp_agent_target::resolve_agent_target(
        &state,
        request.user_id.as_deref(),
        Some(project_id),
        request.session_id.as_deref(),
        None,
    )
    .await?
    {
        return restart_userapp_dev(&state, locale, Some(target), &request).await;
    }

    let user_id = request.user_id.clone();
    let pod_id = request.pod_id.clone();

    // 归属校验：调用方给了 user_id，且 project 已登记归属时，两者必须一致。
    // 否则可以用自己的 user_id + 别人的 project_id 重启他人的 agent。
    // 只在本接口生效；未给 user_id（pod_id 定位）或归属未知时跳过。
    if let Some(claimed) = request
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        && let Some(owner) = state
            .get_project(project_id)
            .and_then(|project| project.user_id().map(str::to_owned))
        && !owner.is_empty()
        && owner != claimed
    {
        restart_log!(
            warn,
            "⛔ [COMPUTER_RESTART] Owner mismatch: project_id={}, claimed user_id={}, owner={}",
            project_id,
            claimed,
            owner
        );
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_COMPUTER_OWNER_CONFLICT,
            locale,
            "project_id 不属于该 user_id，拒绝重启",
        ));
    }

    // 已知并发窗口（不改行为，仅记录）：重启=停止"当前在跑的执行"。
    // gRPC 的 expected_session_id 语义是"**正在运行**的执行的会话"，而注册表里
    // 的 session_id 是 resume 提示（agent 没跑时也保留），拿它当 expected 会被
    // 判成 session_mismatch，把幂等成功打成冲突（实测：期望与当前字面相同仍
    // mismatch）。所以不传 expected；代价是"点击重启到 RPC 到达之间若刚好有
    // 新对话 spawn 了新执行，会被一并停掉"——调用方若需要精确，可显式传
    // session_id（那时语义由调用方保证）。
    restart_log!(
        info,
        "🔁 [COMPUTER_RESTART] Starting to restart Agent: user_id={:?}, pod_id={:?}, project_id={}, session_id={:?}",
        user_id,
        pod_id,
        project_id,
        request.session_id
    );

    // 2. 查找容器：project 映射优先（computer 与 userApp 开发对话都注册映射，
    //    UserappBuilder 开发容器仅存在于映射——user_id/pod_id 的 computer 查找
    //    覆盖不了它）；映射 miss 再按 user_id/pod_id 走 computer 容器查找
    let container_info = if let Some(mapped) = state
        .get_project(project_id)
        .and_then(|p| p.container_info())
    {
        restart_log!(
            info,
            "📦 [COMPUTER_RESTART] Container resolved from project mapping: project_id={}, container_id={}",
            project_id,
            mapped.container_id
        );
        Some(mapped)
    } else if has_user_id {
        let uid = user_id
            .as_ref()
            .ok_or_else(|| AppError::validation_error("user_id is required"))?;
        crate::service::ComputerContainerManager::get_container_info(uid, state.runtime()).await?
    } else {
        // pod_id 作为容器标识符查找
        let pid = pod_id
            .as_ref()
            .ok_or_else(|| AppError::validation_error("pod_id is required"))?;
        crate::service::ComputerContainerManager::get_container_info(pid, state.runtime()).await?
    };

    let container_info = match container_info {
        Some(info) => info,
        None => {
            restart_log!(
                warn,
                "[COMPUTER_RESTART] Container not found: user_id={:?}, pod_id={:?}",
                user_id,
                pod_id
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                locale,
            ));
        }
    };

    restart_log!(
        info,
        "📦 [COMPUTER_RESTART] Container found: container_id={}, ip={}",
        container_info.container_id,
        container_info.container_ip
    );

    // 3. 通过 gRPC 调用 StopAgent RPC（重启 = 停止执行；agent_runner 侧
    //    进程退出确认与缓存清理复用既有链路，gRPC 层零改动）
    restart_log!(
        info,
        "🔄 [COMPUTER_RESTART] Preparing to call StopAgent RPC: project_id={}",
        project_id
    );

    let grpc_addr = extract_grpc_addr(&container_info.service_url)?;
    restart_log!(info, "[COMPUTER_RESTART] gRPC addr: {}", grpc_addr);

    match crate::grpc::grpc_stop_agent_with_pool_and_session(
        &state.grpc_pool,
        &grpc_addr,
        project_id.to_string(),
        Some("User requested restart".to_string()),
        false, // force=false，优雅停止
        request.session_id.clone(),
        None, // 使用默认超时 (GRPC_STOP_AGENT_TIMEOUT_SECS)
    )
    .await
    {
        Ok(response) => {
            restart_log!(
                info,
                "📥 [COMPUTER_RESTART] Received StopAgent response: result={}, success={}",
                response.result,
                response.success
            );

            // 幂等成功的结果集合；错误分支如实返回（不伪造重启成功）。
            match response.result.as_str() {
                "stopped" | "not_found" | "already_stopped" => {
                    // 被重启的会话以 StopAgent 返回的执行身份为准（RPC 内部
                    // 确认停止的 registry 条目）——禁止事后从注册表读取
                    //（停止与读取之间可能换代，读到的是别的会话）。
                    // stopped 时该字段必填；not_found/already_stopped 未停止
                    // 任何执行，回退注册表当前保留值（下次 chat resume 的
                    // 对象；若此刻已换代到新会话 C，恢复对象即 C，陈述为真）。
                    let resumed_session_id = response
                        .stopped_session_id
                        .clone()
                        .filter(|sid| !sid.is_empty())
                        .or_else(|| {
                            state
                                .get_project(project_id)
                                .and_then(|p| p.session_id().map(str::to_owned))
                                .and_then(|sid| {
                                    shared_types::SessionHint::classify(Some(&sid))
                                        .as_session_id()
                                        .map(str::to_owned)
                                })
                        });
                    let message = if response.result == "stopped" {
                        shared_types::get_i18n_message("success.agent_restarted", locale)
                    } else {
                        shared_types::get_i18n_message("success.agent_restart_idempotent", locale)
                    };
                    restart_log!(
                        info,
                        "✅ [COMPUTER_RESTART] Agent restart completed: user_id={:?}, pod_id={:?}, project_id={}, result={}, resumed_session_id={:?}",
                        user_id,
                        pod_id,
                        project_id,
                        response.result,
                        resumed_session_id
                    );
                    return Ok(HttpResult::success(ComputerAgentRestartResponse {
                        success: true,
                        message,
                        user_id,
                        pod_id,
                        project_id: project_id.to_string(),
                        resumed_session_id,
                    }));
                }
                "session_mismatch" => {
                    // 执行已换代：期望停掉的那个执行已经不在（或已换成新会话）。
                    // 原来这里返回 AGENT_NOT_FOUND(404)，会让人误以为"agent 不存在"，
                    // 实际是"你要停的那个已经不是当前这个"。改成冲突语义并带上
                    // 两侧身份，便于前端刷新后重试。
                    let current = state
                        .get_project(project_id)
                        .and_then(|project| project.session_id().map(str::to_owned));
                    restart_log!(
                        warn,
                        "[COMPUTER_RESTART] Session mismatch: project_id={}, expected={:?}, current={:?}",
                        project_id,
                        request.session_id,
                        current
                    );
                    return Ok(HttpResult::error_with_message(
                        shared_types::error_codes::ERR_CONFLICT,
                        locale,
                        &format!(
                            "执行已换代：期望 {}，当前 {}；请刷新后重试",
                            request.session_id.as_deref().unwrap_or("<任意>"),
                            current.as_deref().unwrap_or("<无>")
                        ),
                    ));
                }
                "error" => {
                    let err_msg = response
                        .message
                        .unwrap_or_else(|| "Unknown error".to_string());
                    restart_log!(
                        error,
                        "[COMPUTER_RESTART] Agent restart failed: {}",
                        err_msg
                    );
                    return Ok(HttpResult::error_with_locale(
                        shared_types::error_codes::ERR_STOP_FAILED,
                        locale,
                    ));
                }
                other => {
                    restart_log!(warn, "[COMPUTER_RESTART] unexpected result: {}", other);
                    return Ok(HttpResult::error_with_locale(
                        shared_types::error_codes::ERR_UNKNOWN,
                        locale,
                    ));
                }
            }
        }
        Err(e) => {
            restart_log!(
                error,
                "❌ [COMPUTER_RESTART] StopAgent RPC call failed: {}, project_id={}",
                e,
                project_id
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_GRPC_ERROR,
                locale,
            ));
        }
    }
}

/// userApp dev 分派：重启 UserappBuilder 开发容器内 app 会话的 agent。
///
/// 定位 = project 映射优先（builder 容器注册于 `state.projects[app_id]`）；
/// 映射 miss 时按 UserappBuilder 只读实时查（操作型接口不 ensure 不自愈）。
/// 与 [`crate::handler::computer_agent_stop`] 的 userApp 分支同构，差异：
/// 成功/already_stopped 后**跳过** `clear_userapp_agent_sessions`，保留
/// binding 的会话 ID 供下次开发对话 resume。
async fn restart_userapp_dev(
    state: &AppState,
    locale: &'static str,
    target: Option<super::userapp_agent_target::UserAppAgentTarget>,
    request: &ComputerAgentStopRequest,
) -> Result<HttpResult<ComputerAgentRestartResponse>, AppError> {
    let Some(target) = target else {
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_AGENT_NOT_FOUND,
            locale,
        ));
    };
    let Some(container_info) = target.container_info else {
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
            locale,
        ));
    };
    // RPC 前读取保留会话（不清注册，供下次对话 resume）。userApp 分支身份
    // 天然闭环：省略 session_id 时 RPC expected 传入绑定当前会话，换代即
    // session_mismatch 拒绝——被停止的执行与返回的 resumed 会话恒同身份。
    let resumed_session_id = shared_types::SessionHint::classify(request.session_id.as_deref())
        .as_session_id()
        .or_else(|| {
            shared_types::SessionHint::classify(target.binding.current_session_id.as_deref())
                .as_session_id()
        })
        .map(str::to_owned);
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(shared_types::GRPC_STOP_AGENT_TIMEOUT_SECS);
    let grpc_addr = extract_grpc_addr(&container_info.service_url)?;
    let response = match crate::grpc::grpc_stop_agent_with_pool_and_session(
        &state.grpc_pool,
        &grpc_addr,
        target.binding.project_id.clone(),
        Some("User requested restart".to_string()),
        false,
        request
            .session_id
            .clone()
            .or_else(|| target.binding.current_session_id.clone()),
        Some(deadline.saturating_duration_since(tokio::time::Instant::now())),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            restart_log!(
                error,
                "[COMPUTER_RESTART][USERAPP] StopAgent RPC failed: app_id={}, project_id={}, error={error}",
                target.binding.app_id,
                target.binding.project_id
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_GRPC_ERROR,
                locale,
            ));
        }
    };
    if response.result == "session_mismatch" {
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_AGENT_NOT_FOUND,
            locale,
        ));
    }
    // 结果映射与 computer 路径一致：stopped=执行重启；not_found/already_stopped=
    // 幂等成功（agent 本就不在运行）；session_mismatch=404 已在上文返回；
    // 其余（error/未知）如实失败。
    if !matches!(
        response.result.as_str(),
        "stopped" | "not_found" | "already_stopped"
    ) {
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_STOP_FAILED,
            locale,
        ));
    }
    // 被重启的会话以 StopAgent 返回的执行身份为准;该分支 expected 恒传
    // request.session_id 或绑定当前会话(上文),mismatch 已拒绝——回退预读
    // 值与被停止执行恒同身份。
    let resumed_session_id = response
        .stopped_session_id
        .clone()
        .filter(|sid| !sid.is_empty())
        .or(resumed_session_id);
    let message = if response.result == "stopped" {
        shared_types::get_i18n_message("success.agent_restarted", locale)
    } else {
        shared_types::get_i18n_message("success.agent_restart_idempotent", locale)
    };
    restart_log!(
        info,
        "[COMPUTER_RESTART][USERAPP] restart completed: app_id={}, project_id={}, result={}, resumed_session_id={:?} (binding preserved)",
        target.binding.app_id,
        target.binding.project_id,
        response.result,
        resumed_session_id
    );
    Ok(HttpResult::success(ComputerAgentRestartResponse {
        success: true,
        message,
        user_id: request.user_id.clone(),
        pod_id: None,
        project_id: target.binding.project_id,
        resumed_session_id,
    }))
}

// 与 computer_agent_stop_handler 同款门控:夹具依赖 userapp-turso
//(TursoUserAppStore),缺省 feature 下测试模块整体不编译。
#[cfg(all(test, feature = "userapp-turso"))]
#[path = "computer_agent_restart_handler/tests.rs"]
mod tests;
