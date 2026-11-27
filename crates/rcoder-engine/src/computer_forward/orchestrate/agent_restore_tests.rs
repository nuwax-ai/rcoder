use super::{AgentRestore, ServiceResourceLimits, require_limits_when_rebuilding, wrap};
use container_runtime_api::ContainerRuntimeError;

#[test]
fn idle_user_needs_no_limits_to_be_left_stopped() {
    assert!(require_limits_when_rebuilding("u1", false, None).is_ok());
}

#[test]
fn running_user_with_limits_may_be_stopped_and_rebuilt() {
    let limits = ServiceResourceLimits::default();
    assert!(require_limits_when_rebuilding("u1", true, Some(&limits)).is_ok());
}

/// 反例：切换前 agent 在运行、限额配置缺失——必须在停 agent **之前**拒绝。
/// 停了起不来（wake 路径对同一缺失同样拒绝），只会留下停机。
#[test]
fn running_user_without_limits_is_refused_before_any_stop() {
    let error = require_limits_when_rebuilding("u1", true, None).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("resource_limits"),
        "错误要点名缺的是哪项配置: {message}"
    );
    assert!(message.contains("'u1'"), "错误要点名用户: {message}");
    assert!(
        matches!(error, ContainerRuntimeError::Conflict(_)),
        "配置缺失是确定性拒绝: {error:?}"
    );
}

/// 反例：`wrap` 曾把所有错误压成 K8sError——"门禁被占、稍后重试"到 HTTP 层
/// 变成 500，调用方分不清该重试还是该改配置。
#[test]
fn wrap_keeps_the_variants_that_http_status_depends_on() {
    let preserved = [
        (
            ContainerRuntimeError::Conflict("already migrated".to_string()),
            "already migrated",
            "must stay Conflict",
        ),
        (
            ContainerRuntimeError::ConnectionError("gate held; retry shortly".to_string()),
            "gate held; retry shortly",
            "must stay ConnectionError",
        ),
        (
            ContainerRuntimeError::ConfigurationError("image not configured".to_string()),
            "image not configured",
            "must stay ConfigurationError",
        ),
    ];
    for (error, payload, expectation) in preserved {
        let before = format!("{error:?}");
        let wrapped = wrap("migration_gate", error);
        let after = format!("{wrapped:?}");
        assert!(
            after.starts_with(before.split('(').next().unwrap_or_default()),
            "{expectation}: {before} -> {after}"
        );
        assert!(
            after.contains(&format!(
                "migration step 'migration_gate' failed: {payload}"
            )),
            "步骤名要带进去: {after}"
        );
    }

    // 自带 HTTP 语义、载荷不是字符串的变体原样透传。agent 正在启动（StatefulSet
    // 在、Pod 还没就绪）时 find_container 会给出前两个；此前被压成 K8sError，
    // 可重试的状态变成了 500。
    assert!(matches!(
        wrap(
            "agent_present_check",
            ContainerRuntimeError::ManagementNotRunning
        ),
        ContainerRuntimeError::ManagementNotRunning
    ));
    assert!(matches!(
        wrap(
            "agent_present_check",
            ContainerRuntimeError::OperationInProgress(Box::new(
                shared_types::UserAppOperationInProgress {
                    app_id: "app-1".to_string(),
                    service_type: shared_types::ServiceType::ComputerAgentRunner,
                    resource_name: "rcoder-computer-agent-runner-u1".to_string(),
                    operation_id: None,
                }
            ))
        ),
        ContainerRuntimeError::OperationInProgress(_)
    ));
    assert!(matches!(
        wrap(
            "agent_present_check",
            ContainerRuntimeError::RequestRejected(shared_types::RuntimeRequestRejection {
                status: 409,
                message: "busy".to_string(),
            })
        ),
        ContainerRuntimeError::RequestRejected(ref rejection) if rejection.status == 409
    ));

    // 上游失败类变体仍归并为 K8sError（没有更具体的 HTTP 语义），文案不变
    let error = wrap(
        "stop_agent",
        ContainerRuntimeError::ContainerStopError("boom".to_string()),
    );
    let ContainerRuntimeError::K8sError(message) = error else {
        panic!("上游失败应归并为 K8sError，实际是 {error:?}");
    };
    assert_eq!(
        message,
        "migration step 'stop_agent' failed: Container stop failed: boom"
    );
}

/// 反例（部署评审）：编排锁冲突是"稍后重试"，AppError 的状态码是 503；此前
/// 投影成 K8sError，到 HTTP 层变成 500，与文档承诺的 503 不符。
#[test]
fn app_errors_keep_their_http_semantics_when_projected() {
    use crate::AppError;
    use axum::http::StatusCode;
    use shared_types::error_codes::*;

    let cases = [
        // 锁冲突："稍后重试"
        (
            AppError::with_message(
                ERR_CONTAINER_ADDRESS_NOT_READY,
                "Another request is still ensuring this container; retry later",
            ),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            AppError::with_message(ERR_OPERATION_IN_PROGRESS, "blocked"),
            StatusCode::CONFLICT,
        ),
        (
            AppError::with_message(ERR_CONTAINER_NOT_FOUND, "gone"),
            StatusCode::NOT_FOUND,
        ),
        (
            AppError::with_message(ERR_RUNTIME_TIMEOUT, "slow"),
            StatusCode::GATEWAY_TIMEOUT,
        ),
        (
            AppError::with_message(ERR_BACKEND_ERROR, "upstream"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ];
    for (error, expected) in cases {
        let own_status = error.status_code();
        let projected = super::runtime_error_from_app(error);
        let status = shared_types::error_status_from_code(
            container_runtime_api::runtime_error_code(&projected).0,
        );
        assert_eq!(
            status, own_status,
            "AppError 自己的状态码要原样带到 HTTP 层: {projected:?}"
        );
        assert_eq!(status, expected, "{projected:?}");
    }

    // 运行时错误里没有 400 的对应变体：对不上的维持 K8sError（500）
    let projected =
        super::runtime_error_from_app(AppError::with_message(ERR_VALIDATION, "bad input"));
    assert!(
        matches!(projected, ContainerRuntimeError::K8sError(_)),
        "{projected:?}"
    );
}

#[test]
fn running_agent_is_restarted_whatever_the_later_observation() {
    assert_eq!(AgentRestore::decide(true, false), AgentRestore::Restart);
    assert_eq!(AgentRestore::decide(true, true), AgentRestore::Restart);
}

#[test]
fn idle_agent_stays_stopped() {
    assert_eq!(
        AgentRestore::decide(false, false),
        AgentRestore::KeepStopped
    );
}

#[test]
fn agent_woken_during_cutover_is_replaced() {
    assert_eq!(
        AgentRestore::decide(false, true),
        AgentRestore::ReplaceWoken
    );
}
