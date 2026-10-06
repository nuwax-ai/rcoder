//! 应用管理处理器（facade——按接口组拆分到子模块）
//!
//! 子模块划分：
//! - [`state`]：处理器共享状态 [`AppManagerState`]
//! - [`lifecycle`]：create / query / get / update / delete
//! - [`ops`]：start / stop / restart
//! - [`query`]：health / stats / events
//! - [`files`]：upload / list / delete
//! - [`storage`]：get / clear / destroy / query（v2 §5.4）
//!
//! 数据库管理原属 `db` 子模块（`{app_id}/db/*`），已按拍板下线——统一走
//! rcoder 转发层的 `/api/v1/userapp/db/{app_stage}/*`（app_stage 双环境 + username upsert
//! + dbx 同步的超集实现，见 rcoder userapp_forward::db）。

pub mod control;
pub use control::*;
pub mod files;
pub mod lifecycle;
pub mod logs;
pub mod ops;
pub mod query;
pub mod state;
pub mod storage;

/// 路径段 `{app_stage}` 解析（非法统一 400 + 全站一致文案）。
///
/// handlers 十三处同款三行模式收敛：UserappStage::parse + ok_or_else(bad_request
/// (invalid_app_stage_error))。
pub(super) fn parse_app_stage_param(
    raw: &str,
) -> Result<shared_types::UserappStage, shared_types::AppError> {
    shared_types::UserappStage::parse(raw).ok_or_else(|| {
        shared_types::AppError::bad_request(&shared_types::invalid_app_stage_error(raw))
    })
}

pub use files::*;
pub use lifecycle::*;
pub use logs::*;
pub use ops::*;
pub use query::*;
pub use state::AppManagerState;
pub use storage::*;

// health 信息已合并到 AppRuntimeInfo.health（由 build_runtime_info 经 health_from_status 统一派生）；
// get_app_health 直接取 runtime.health，无需 handler 重复派生（消除 m1 重复）。

/// app 操作错误 → HTTP 响应错误（v2 §12）。
///
/// service 层返回强类型 [`crate::error::AppOperationError`]（variant 携带错误码），handler 通过 From
/// 直接转换——错误码在 service 抛出点确定（Fail Fast），无需 downcast / 字符串匹配。
impl From<crate::error::AppOperationError> for shared_types::AppError {
    fn from(e: crate::error::AppOperationError) -> Self {
        let in_progress_data = e.operation_in_progress_data();
        let message = e.message().to_string();
        let e_code = e.code().to_owned();
        let error = shared_types::AppError::with_message(&e_code, message.clone());
        let mapped = match e {
            crate::error::AppOperationError::CredentialApplication {
                diagnostic: Some(mut detail),
                ..
            } => {
                detail.code = e_code.into();
                detail.retryable = false;
                detail.into_app_error()
            }
            crate::error::AppOperationError::CredentialApplication { mutation, .. } => error
                .with_error_detail(
                    shared_types::ErrorDetail::new(
                        if mutation == shared_types::CredentialMutationEvidence::NotAttempted {
                            shared_types::ERR_DATABASE_COMMAND_FAILED
                        } else {
                            shared_types::ERR_OPERATION_OUTCOME_UNKNOWN
                        },
                        "database_credentials",
                        message,
                    )
                    .with_retryable(false),
                ),
            crate::error::AppOperationError::Diagnostic(detail) => detail.into_app_error(),
            crate::error::AppOperationError::Operation {
                operation_id,
                source,
            } => Self::from(*source).with_operation_id(operation_id),
            crate::error::AppOperationError::ConflictBlocked { blocker, .. } => {
                error.with_blocker(blocker)
            }
            crate::error::AppOperationError::OperationInProgress { blocker, data, .. } => {
                let error = error.with_operation_in_progress_data(*data);
                match blocker {
                    Some(blocker) => error.with_blocker(*blocker),
                    None => error,
                }
            }
            crate::error::AppOperationError::NotFound(_)
            | crate::error::AppOperationError::OperationNotFound(_)
            | crate::error::AppOperationError::AlreadyExists(_)
            | crate::error::AppOperationError::InvalidState(_)
            | crate::error::AppOperationError::FileNotFound(_)
            | crate::error::AppOperationError::Validation(_)
            | crate::error::AppOperationError::DevNotRunning(_)
            | crate::error::AppOperationError::Backend(_)
            | crate::error::AppOperationError::RuntimeRejected(_)
            | crate::error::AppOperationError::Conflict(_)
            | crate::error::AppOperationError::HotDeployEnvChange(_) => error,
        };
        match in_progress_data {
            Some(data) => mapped.with_operation_in_progress_data(data),
            None => mapped,
        }
    }
}

#[cfg(test)]
mod credential_carrier_boundary_tests {
    #[test]
    fn credential_http_error_keeps_child_diagnostic_under_captured_parent_identity() {
        let blocker = shared_types::UserAppOperationBlocker {
            scope: shared_types::UserAppOperationScope::Prod,
            operation_id: "downstream-blocking-stop".into(),
            kind: shared_types::UserAppOperationKind::Stop,
            state: shared_types::UserAppOperationState::Running,
            step: "stop".into(),
        };
        let source = crate::AppOperationError::CredentialApplication {
            message: "Credential write completion remains unconfirmed".into(),
            mutation: shared_types::CredentialMutationEvidence::Unknown,
            diagnostic: Some(Box::new(shared_types::WakeFailure {
                cause_code: shared_types::ERR_RUNTIME_TIMEOUT.into(),
                command_diagnostic: Some(Box::new(shared_types::PgCommandDiagnostic {
                    code: shared_types::ERR_RUNTIME_TIMEOUT.into(),
                    operation_id: Some("original-downstream-db-operation".into()),
                    blocker: Some(blocker.clone()),
                    error_detail: Some(
                        shared_types::ErrorDetail::new(
                            shared_types::ERR_RUNTIME_TIMEOUT,
                            "original_db_response",
                            "Original database response timed out",
                        )
                        .with_task_id("real-downstream-db-task")
                        .with_service_id("postgres")
                        .with_retryable(true),
                    ),
                })),
                ..shared_types::WakeFailure::new(
                    shared_types::ERR_RUNTIME_TIMEOUT,
                    "credential_write",
                    "Credential write completion remains unconfirmed",
                )
            })),
        }
        .with_operation_id("captured-parent-reset-password".into());
        assert!(source.requires_recovery());
        let response = shared_types::AppError::from(source).into_http_result::<()>("en-US");
        assert_eq!(response.code, shared_types::ERR_RECOVERY_REQUIRED);
        assert_eq!(
            response.operation_id.as_deref(),
            Some("captured-parent-reset-password")
        );
        assert_eq!(response.blocker, Some(blocker));
        let detail = response
            .error_detail
            .expect("original downstream diagnostic");
        assert_eq!(detail.task_id.as_deref(), Some("real-downstream-db-task"));
        assert_eq!(detail.service_id.as_deref(), Some("postgres"));
        assert_eq!(detail.reason_code, shared_types::ERR_RUNTIME_TIMEOUT);
        assert_eq!(detail.stage, "original_db_response");
        assert!(
            !detail.retryable,
            "downstream observation cannot release the parent's unknown-write protection"
        );
    }
}
