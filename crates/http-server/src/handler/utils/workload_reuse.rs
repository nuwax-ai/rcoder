//! `refresh_agent_workload_fence` 失败来源的统一穷尽映射（chat / computer
//! chat / pod ensure 三处共用）。新增 `AgentWorkloadFenceError` 变体会迫使
//! 本 match 补齐，不再静默落入通用后端错误。

use rcoder_engine::app_state::{AgentWorkloadFenceError, AppState};
use rcoder_storage::AdmissionVerifyError;
use shared_types::AppError;
use shared_types::error_codes as ec;

pub(crate) fn refresh_fence_error_to_app_error(error: AgentWorkloadFenceError) -> AppError {
    match error {
        // Runtime 原因委托既有 mapper：Conflict/OperationInProgress → ERR_CONFLICT，
        // 只读不可达/超时 → ERR_RUNTIME_*（retryable），保持 stage 语义不变。
        AgentWorkloadFenceError::Runtime(cause) => container_runtime_api::runtime_app_error(
            &cause,
            "workload.reuse",
            container_runtime_api::RuntimeErrorContext::ReadOnly,
        ),
        // 准入与目标不符：重试同一 ensure 请求也无法自愈，按冲突暴露
        AgentWorkloadFenceError::Target(_) => AppError::with_message(
            ec::ERR_CONFLICT,
            "Workload admission does not cover the captured Agent target; retry the request",
        ),
        AgentWorkloadFenceError::AdmissionBeforeRuntime { source } => {
            admission_unconfirmed_app_error("before", source)
        }
        AgentWorkloadFenceError::AdmissionAfterRuntime { source } => {
            admission_unconfirmed_app_error("after", source)
        }
    }
}

pub(crate) fn admission_unconfirmed_app_error(
    stage: &str,
    source: AdmissionVerifyError,
) -> AppError {
    match source {
        // 会话已丢失/关闭：与 ensure 入口的既有语义对齐（ERR_CONFLICT，重试重取准入）
        AdmissionVerifyError::SessionLost | AdmissionVerifyError::SessionClosed => {
            AppError::with_message(
                ec::ERR_CONFLICT,
                format!(
                    "Workload admission session was lost {stage} the runtime fence refresh; retry later"
                ),
            )
        }
        // 只读探针回复未知：不是 SQL 写入结果未知，不得使用
        // ERR_OPERATION_OUTCOME_UNKNOWN；也不据此放行未知执行
        AdmissionVerifyError::OutcomeUnknown => AppError::with_message(
            ec::ERR_BACKEND_ERROR,
            format!(
                "Workload reuse could not be confirmed {stage} the runtime fence refresh; admission verify outcome is unknown; retry later"
            ),
        ),
        AdmissionVerifyError::ProbeFailed(source) => AppError::with_message(
            ec::ERR_BACKEND_ERROR,
            format!(
                "Workload reuse could not be confirmed {stage} the runtime fence refresh: {}",
                shared_types::sanitize_error_text(&source.to_string())
            ),
        ),
    }
}

/// 三处调用点的公共形态：刷新 fence 并把类型化错误映射为对外 HTTP 错误。
pub(crate) async fn refresh_workload_fence(
    state: &AppState,
    admission: &rcoder_storage::WorkloadAdmissionGuard,
    identifier: &str,
    service_type: &shared_types::ServiceType,
) -> Result<(), AppError> {
    state
        .refresh_agent_workload_fence(admission, identifier, service_type)
        .await
        .map_err(refresh_fence_error_to_app_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcoder_engine::app_state::AgentAdmissionTargetError;
    use shared_types::error_codes as ec;

    /// Runtime Conflict 经既有 mapper → ERR_CONFLICT（ReadOnly 无 unknown 升级）。
    #[test]
    fn runtime_conflict_maps_through_existing_runtime_mapper() {
        let error = refresh_fence_error_to_app_error(AgentWorkloadFenceError::Runtime(
            container_runtime_api::ContainerRuntimeError::Conflict("replacement in flight".into()),
        ));
        let result = error.into_http_result::<()>("en-US");
        assert_eq!(result.code, ec::ERR_CONFLICT);
        assert!(result.operation_id.is_none());
    }

    /// Runtime 连接失败（只读）→ ERR_RUNTIME_UNAVAILABLE 且 retryable。
    #[test]
    fn runtime_connection_failure_is_retryable_read_error() {
        let error = refresh_fence_error_to_app_error(AgentWorkloadFenceError::Runtime(
            container_runtime_api::ContainerRuntimeError::ConnectionError("dial refused".into()),
        ));
        let result = error.into_http_result::<()>("en-US");
        assert_eq!(result.code, ec::ERR_RUNTIME_UNAVAILABLE);
        assert!(result.error_detail.unwrap().retryable);
    }

    /// 准入目标不符 → ERR_CONFLICT，不再是通用后端错误。
    #[test]
    fn admission_target_mismatch_is_a_conflict() {
        let error = refresh_fence_error_to_app_error(AgentWorkloadFenceError::Target(
            AgentAdmissionTargetError::Mismatch,
        ));
        let result = error.into_http_result::<()>("en-US");
        assert_eq!(result.code, ec::ERR_CONFLICT);
        assert!(
            result
                .message
                .contains("does not cover the captured Agent target")
        );
    }

    /// 会话丢失/关闭 → ERR_CONFLICT（与 ensure 入口语义对齐；原实现统一
    /// ERR_BACKEND_ERROR，属本契约调整的行为变化点）。
    #[test]
    fn admission_session_loss_is_retryable_conflict() {
        for stage in ["before", "after"] {
            for kind in ["lost", "closed"] {
                let source = match kind {
                    "lost" => AdmissionVerifyError::SessionLost,
                    _ => AdmissionVerifyError::SessionClosed,
                };
                let error = match stage {
                    "before" => AgentWorkloadFenceError::AdmissionBeforeRuntime { source },
                    _ => AgentWorkloadFenceError::AdmissionAfterRuntime { source },
                };
                let result =
                    refresh_fence_error_to_app_error(error).into_http_result::<()>("en-US");
                assert_eq!(result.code, ec::ERR_CONFLICT, "{stage}/{kind}");
                assert!(
                    result.message.contains(stage),
                    "{stage}/{kind}: {}",
                    result.message
                );
            }
        }
    }

    /// PG 探针回复未知：保持 ERR_BACKEND_ERROR（只读探针的回复未知，不是
    /// SQL 写入结果未知），且消息可区分 before/after 阶段。
    #[test]
    fn admission_outcome_unknown_stays_backend_error_not_write_unknown() {
        for (stage, error) in [
            (
                "before",
                AgentWorkloadFenceError::AdmissionBeforeRuntime {
                    source: AdmissionVerifyError::OutcomeUnknown,
                },
            ),
            (
                "after",
                AgentWorkloadFenceError::AdmissionAfterRuntime {
                    source: AdmissionVerifyError::OutcomeUnknown,
                },
            ),
        ] {
            let result = refresh_fence_error_to_app_error(error).into_http_result::<()>("en-US");
            assert_eq!(result.code, ec::ERR_BACKEND_ERROR, "{stage}");
            assert_ne!(result.code, ec::ERR_OPERATION_OUTCOME_UNKNOWN, "{stage}");
            assert!(
                result
                    .message
                    .contains("admission verify outcome is unknown"),
                "{stage}"
            );
            assert!(result.message.contains(stage), "{stage}");
        }
    }

    /// 探针自身失败 → ERR_BACKEND_ERROR，且不回显未净化文本。
    #[test]
    fn admission_probe_failure_sanitizes_source() {
        let error =
            refresh_fence_error_to_app_error(AgentWorkloadFenceError::AdmissionAfterRuntime {
                source: AdmissionVerifyError::ProbeFailed(anyhow::anyhow!(
                    "select 1 failed: postgres://user:secret@host/db"
                )),
            });
        let result = error.into_http_result::<()>("en-US");
        assert_eq!(result.code, ec::ERR_BACKEND_ERROR);
        assert!(!result.message.contains("secret"), "{}", result.message);
    }
}
