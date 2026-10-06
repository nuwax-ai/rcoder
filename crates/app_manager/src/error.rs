//! app_manager service 层错误类型。
//!
//! 从 models.rs 抽出——错误类型与数据模型职责不同（SRP）：service 抛出强类型错误，
//! handler 用 `impl From<AppOperationError> for AppError` 精确映射 HTTP。
//! models.rs 经 `pub use crate::error::{AppOperationError, AppResult};` re-export，
//! 故 `super::models::*` 与 crate 根 `app_manager::AppOperationError` 均可达（零调用方改动）。

use std::fmt;

use shared_types::error_codes::{
    ERR_APP_ALREADY_EXISTS, ERR_APP_NOT_FOUND, ERR_BACKEND_ERROR, ERR_CONFLICT,
    ERR_DEV_NOT_RUNNING, ERR_FILE_NOT_FOUND, ERR_INVALID_STATE, ERR_NOT_FOUND, ERR_VALIDATION,
};

/// app 操作级错误（携带业务错误码，供 handler 精确映射 HTTP）。
///
/// 每个错误场景一个 variant，`code()`/`message()` 用 match 实现，编译器强制穷举
/// （新增 variant 时所有 match 编译报错，OCP）。message 含完整因果链，由 service
/// 层在构造时拼入。handler 通过 `impl From<AppOperationError> for AppError` 直接转换，
/// 无需 downcast / 字符串匹配。
#[derive(Debug)]
pub enum AppOperationError {
    /// 应用不存在（404 ERR_APP_NOT_FOUND）
    NotFound(String),
    /// An operation or another non-application resource is absent (404 ERR_NOT_FOUND).
    OperationNotFound(String),
    /// 应用已存在（409 ERR_APP_ALREADY_EXISTS）
    AlreadyExists(String),
    /// 操作状态非法，如未 delete 就清空存储（409 ERR_INVALID_STATE）
    InvalidState(String),
    /// 文件/目录不存在（404 ERR_FILE_NOT_FOUND）
    FileNotFound(String),
    /// 请求参数校验失败（400 ERR_VALIDATION）
    Validation(String),
    /// dev 会话未运行（400 ERR_DEV_NOT_RUNNING）——dev 日志受理前置检查快速
    /// 失败；启动 dev 会话后即可查询
    DevNotRunning(String),
    /// 后端运行时错误（500 ERR_BACKEND_ERROR，兜底）
    Backend(String),
    /// Structured diagnostic preserved across wake single-flight and HTTP.
    Diagnostic(shared_types::WakeFailure),
    /// Keep mutation evidence across service/HTTP error mapping. A failed TCP
    /// verification after ALTER must not be treated as a pre-mutation rejection.
    CredentialApplication {
        message: String,
        mutation: shared_types::CredentialMutationEvidence,
        diagnostic: Option<Box<shared_types::WakeFailure>>,
    },
    /// One remote HTTP request was explicitly rejected. This does not prove a
    /// multi-request operation had no earlier effects; callers retain that boundary.
    RuntimeRejected(shared_types::RuntimeRequestRejection),
    /// 乐观锁冲突（409 ERR_CONFLICT）—— expected_resource_version 不匹配
    Conflict(String),
    /// 受理被在途操作阻塞（409 ERR_OPERATION_IN_PROGRESS）——携带结构化 blocker，
    /// 指名阻塞 scope/kind/state，随错误信封透出供调用方分支
    ConflictBlocked {
        message: String,
        blocker: shared_types::UserAppOperationBlocker,
    },
    /// Another holder occupies this application. Its identity is independent
    /// from an admitted caller's correlation and may be genuinely unknown.
    OperationInProgress {
        message: String,
        blocker: Option<Box<shared_types::UserAppOperationBlocker>>,
        data: Box<shared_types::OperationInProgressData>,
    },
    HotDeployEnvChange(String),
    /// Identity from a durable admission receipt. This adds correlation only;
    /// the underlying error still decides its code and recovery requirements.
    Operation {
        operation_id: String,
        source: Box<AppOperationError>,
    },
}

impl AppOperationError {
    pub(crate) fn operation_in_progress(
        blocker: Option<shared_types::UserAppOperationBlocker>,
        data: shared_types::OperationInProgressData,
    ) -> Self {
        let holder = data.holder_operation_id.as_deref().unwrap_or("unknown");
        Self::OperationInProgress {
            message: format!("Application operation is in progress (holder {holder})"),
            blocker: blocker.map(Box::new),
            data: Box::new(data),
        }
    }

    /// An admitted caller never acquires retry permission from its blocker.
    pub fn operation_in_progress_data(&self) -> Option<shared_types::OperationInProgressData> {
        let mut data = match self.root_cause() {
            Self::OperationInProgress { data, .. } => (**data).clone(),
            Self::ConflictBlocked { blocker, .. } => {
                shared_types::OperationInProgressData::from_blocker(blocker, false, false, 0)
            }
            _ => return None,
        };
        if self.operation_id().is_some() {
            data.retryable = false;
            data.retry_after_seconds = 0;
        }
        Some(data)
    }

    pub(crate) fn with_operation_id(self, operation_id: String) -> Self {
        // Preserve the first known operation rather than replacing it with a
        // caller's later lookup or a newly generated candidate identity.
        if self.operation_id().is_some() {
            return self;
        }
        Self::Operation {
            operation_id,
            source: Box::new(self),
        }
    }

    pub fn operation_id(&self) -> Option<&str> {
        match self {
            Self::Operation { operation_id, .. } => Some(operation_id),
            Self::Diagnostic(detail) => detail.operation_id.as_deref(),
            Self::NotFound(_)
            | Self::OperationNotFound(_)
            | Self::AlreadyExists(_)
            | Self::InvalidState(_)
            | Self::FileNotFound(_)
            | Self::Validation(_)
            | Self::DevNotRunning(_)
            | Self::Backend(_)
            | Self::CredentialApplication { .. }
            | Self::RuntimeRejected(_)
            | Self::Conflict(_)
            | Self::ConflictBlocked { .. }
            | Self::OperationInProgress { .. }
            | Self::HotDeployEnvChange(_) => None,
        }
    }

    pub fn root_cause(&self) -> &Self {
        match self {
            Self::Operation { source, .. } => source.root_cause(),
            Self::Diagnostic(_)
            | Self::NotFound(_)
            | Self::OperationNotFound(_)
            | Self::AlreadyExists(_)
            | Self::InvalidState(_)
            | Self::FileNotFound(_)
            | Self::Validation(_)
            | Self::DevNotRunning(_)
            | Self::Backend(_)
            | Self::CredentialApplication { .. }
            | Self::RuntimeRejected(_)
            | Self::Conflict(_)
            | Self::ConflictBlocked { .. }
            | Self::OperationInProgress { .. }
            | Self::HotDeployEnvChange(_) => self,
        }
    }

    pub(crate) fn requires_recovery(&self) -> bool {
        match self {
            Self::Operation { source, .. } => source.requires_recovery(),
            Self::Diagnostic(detail) => {
                detail.code.as_ref() == shared_types::ERR_OPERATION_OUTCOME_UNKNOWN
            }
            Self::CredentialApplication { mutation, .. } => match mutation {
                shared_types::CredentialMutationEvidence::Unknown
                | shared_types::CredentialMutationEvidence::AppliedButUnverified => true,
                shared_types::CredentialMutationEvidence::NotAttempted => false,
            },
            Self::NotFound(_)
            | Self::OperationNotFound(_)
            | Self::AlreadyExists(_)
            | Self::InvalidState(_)
            | Self::FileNotFound(_)
            | Self::Validation(_)
            | Self::DevNotRunning(_)
            | Self::Backend(_)
            | Self::RuntimeRejected(_)
            | Self::Conflict(_)
            | Self::ConflictBlocked { .. }
            | Self::OperationInProgress { .. }
            | Self::HotDeployEnvChange(_) => false,
        }
    }

    pub(crate) fn wake_failure(&self, stage: &'static str) -> shared_types::WakeFailure {
        let mut failure = match self.root_cause() {
            Self::Diagnostic(detail) => detail.clone(),
            Self::CredentialApplication {
                diagnostic: Some(detail),
                ..
            } => {
                let mut detail = (**detail).clone();
                detail.code = self.code().into();
                detail
            }
            _ => shared_types::WakeFailure::new(self.code(), stage, self.message()),
        };
        failure.operation_id = self
            .operation_id()
            .map(str::to_owned)
            .or(failure.operation_id);
        if let Self::ConflictBlocked { blocker, .. } = self.root_cause() {
            failure.blocker = Some(Box::new(blocker.clone()));
        }
        if let Self::OperationInProgress {
            blocker: Some(blocker),
            ..
        } = self.root_cause()
        {
            failure.blocker = Some(blocker.clone());
        }
        failure
    }

    pub(crate) fn credential_failure(error: shared_types::AlignError, password: &str) -> Self {
        let mutation = error.mutation_evidence();
        let message = if password.is_empty() {
            error.to_string()
        } else {
            error.to_string().replace(password, "[REDACTED]")
        };
        match error {
            shared_types::AlignError::InvalidInput(_)
            | shared_types::AlignError::RoleMissing(_) => Self::Validation(message),
            shared_types::AlignError::Command {
                code,
                cause_code,
                stage,
                diagnostic,
                ..
            } => {
                if mutation == shared_types::CredentialMutationEvidence::NotAttempted {
                    Self::Diagnostic(shared_types::WakeFailure {
                        cause_code: cause_code.into(),
                        command_diagnostic: diagnostic,
                        ..shared_types::WakeFailure::new(code, stage, message)
                    })
                } else {
                    Self::CredentialApplication {
                        diagnostic: Some(Box::new(shared_types::WakeFailure {
                            cause_code: cause_code.into(),
                            command_diagnostic: diagnostic,
                            ..shared_types::WakeFailure::new(code, stage, message.clone())
                        })),
                        message,
                        mutation,
                    }
                }
            }
        }
    }

    pub(crate) fn credential_command_error(
        error: shared_types::PgCommandError,
        stage: &'static str,
        mutation: shared_types::CredentialMutationEvidence,
        message: &'static str,
    ) -> Self {
        let failure = shared_types::WakeFailure {
            cause_code: error.cause_code.into(),
            command_diagnostic: error.diagnostic,
            ..shared_types::WakeFailure::new(error.code, stage, message)
        };
        Self::CredentialApplication {
            message: message.into(),
            mutation,
            diagnostic: Some(Box::new(failure)),
        }
    }

    /// 业务错误码（ERR_* 常量）
    pub fn code(&self) -> &str {
        match self {
            Self::Diagnostic(detail) => detail.code.as_ref(),
            Self::NotFound(_) => ERR_APP_NOT_FOUND,
            Self::OperationNotFound(_) => ERR_NOT_FOUND,
            Self::AlreadyExists(_) => ERR_APP_ALREADY_EXISTS,
            Self::InvalidState(_) => ERR_INVALID_STATE,
            Self::FileNotFound(_) => ERR_FILE_NOT_FOUND,
            Self::Validation(_) => ERR_VALIDATION,
            Self::DevNotRunning(_) => ERR_DEV_NOT_RUNNING,
            Self::Backend(_) => ERR_BACKEND_ERROR,
            Self::CredentialApplication { .. } if self.requires_recovery() => {
                shared_types::ERR_RECOVERY_REQUIRED
            }
            Self::CredentialApplication {
                diagnostic: Some(detail),
                ..
            } => detail.code.as_ref(),
            Self::CredentialApplication { .. } => shared_types::ERR_DATABASE_COMMAND_FAILED,
            Self::RuntimeRejected(rejection) if rejection.status == 409 => ERR_CONFLICT,
            Self::RuntimeRejected(_) => ERR_BACKEND_ERROR,
            Self::Conflict(_) => ERR_CONFLICT,
            Self::ConflictBlocked { .. } | Self::OperationInProgress { .. } => {
                shared_types::ERR_OPERATION_IN_PROGRESS
            }
            Self::HotDeployEnvChange(_) => shared_types::error_codes::ERR_HOT_DEPLOY_ENV_CHANGE,
            Self::Operation { source, .. } => source.code(),
        }
    }

    /// 人读错误信息（含完整因果链，由 service 构造时拼入）
    pub fn message(&self) -> &str {
        match self {
            Self::Operation { source, .. } => source.message(),
            Self::Diagnostic(detail) => &detail.message,
            Self::RuntimeRejected(rejection) => &rejection.message,
            Self::ConflictBlocked { message, .. }
            | Self::OperationInProgress { message, .. }
            | Self::CredentialApplication { message, .. } => message,
            Self::NotFound(m)
            | Self::OperationNotFound(m)
            | Self::AlreadyExists(m)
            | Self::InvalidState(m)
            | Self::FileNotFound(m)
            | Self::Validation(m)
            | Self::DevNotRunning(m)
            | Self::Backend(m)
            | Self::Conflict(m)
            | Self::HotDeployEnvChange(m) => m,
        }
    }
}

impl fmt::Display for AppOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code(), self.message())
    }
}

impl std::error::Error for AppOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Operation { source, .. } => Some(source.as_ref()),
            Self::Diagnostic(_)
            | Self::NotFound(_)
            | Self::OperationNotFound(_)
            | Self::AlreadyExists(_)
            | Self::InvalidState(_)
            | Self::FileNotFound(_)
            | Self::Validation(_)
            | Self::DevNotRunning(_)
            | Self::Backend(_)
            | Self::CredentialApplication { .. }
            | Self::RuntimeRejected(_)
            | Self::Conflict(_)
            | Self::ConflictBlocked { .. }
            | Self::OperationInProgress { .. }
            | Self::HotDeployEnvChange(_) => None,
        }
    }
}

impl From<shared_types::UserAppStoreError> for AppOperationError {
    fn from(error: shared_types::UserAppStoreError) -> Self {
        use shared_types::UserAppStoreError;
        match error {
            UserAppStoreError::OperationInProgress(blocker) => {
                let data =
                    shared_types::OperationInProgressData::from_blocker(&blocker, false, false, 0);
                Self::operation_in_progress(Some(blocker), data)
            }
            UserAppStoreError::OwnershipConflict
            | UserAppStoreError::LifecycleConflict
            | UserAppStoreError::VersionConflict => Self::Conflict(error.to_string()),
            UserAppStoreError::NotFound => Self::OperationNotFound(error.to_string()),
            UserAppStoreError::InvalidOperation(_) => Self::InvalidState(error.to_string()),
            UserAppStoreError::Storage(source) => {
                if let Some(unknown) =
                    source.downcast_ref::<shared_types::OperationOutcomeUnknown>()
                {
                    Self::Diagnostic(shared_types::WakeFailure::new(
                        shared_types::ERR_OPERATION_OUTCOME_UNKNOWN,
                        unknown.stage,
                        &unknown.detail,
                    ))
                } else {
                    Self::Backend(format!("Application storage failed: {source:#}"))
                }
            }
        }
    }
}

/// app service 操作返回类型
pub type AppResult<T> = Result<T, AppOperationError>;

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn operation_error_keeps_diagnostic_payload_bounded() {
        assert!(
            size_of::<AppOperationError>() <= 128,
            "AppOperationError is {} bytes; inline diagnostic payload must stay bounded",
            size_of::<AppOperationError>()
        );
    }

    #[test]
    fn credential_error_keeps_evidence_and_redacts_password() {
        for mutation in [
            shared_types::CredentialMutationEvidence::NotAttempted,
            shared_types::CredentialMutationEvidence::Unknown,
            shared_types::CredentialMutationEvidence::AppliedButUnverified,
        ] {
            let error = AppOperationError::credential_failure(
                shared_types::AlignError::Command {
                    code: shared_types::ERR_DATABASE_COMMAND_FAILED,
                    cause_code: shared_types::ERR_DATABASE_COMMAND_FAILED,
                    stage: "credential step",
                    detail: "failed with privatepassword".into(),
                    mutation,
                    diagnostic: None,
                },
                "privatepassword",
            );
            assert!(!error.to_string().contains("privatepassword"));
            assert_eq!(
                error.requires_recovery(),
                mutation != shared_types::CredentialMutationEvidence::NotAttempted
            );
            if error.requires_recovery() {
                assert_eq!(error.code(), shared_types::ERR_RECOVERY_REQUIRED);
            }
            if mutation == shared_types::CredentialMutationEvidence::NotAttempted {
                assert!(matches!(error, AppOperationError::Diagnostic(_)));
            } else {
                assert!(
                    matches!(error, AppOperationError::CredentialApplication { mutation: actual, .. } if actual == mutation)
                );
            }
        }
    }

    #[test]
    fn operation_context_keeps_original_identity_code_and_recovery() {
        let cause = AppOperationError::CredentialApplication {
            diagnostic: None,
            message: "Credential verification failed — outcome unknown".into(),
            mutation: shared_types::CredentialMutationEvidence::Unknown,
        };
        let message = cause.message().to_owned();
        let code = cause.code().to_owned();
        let error = cause
            .with_operation_id("accepted-operation".into())
            .with_operation_id("later-operation".into());
        assert_eq!(error.operation_id(), Some("accepted-operation"));
        assert_eq!(error.code(), code);
        assert_eq!(error.message(), message);
        assert!(error.requires_recovery());
        assert!(matches!(
            error.root_cause(),
            AppOperationError::CredentialApplication { .. }
        ));
    }

    #[test]
    fn operation_context_preserves_independent_conflict_blocker() {
        let error = AppOperationError::ConflictBlocked {
            message: "Original scope conflict".into(),
            blocker: shared_types::UserAppOperationBlocker {
                scope: shared_types::UserAppOperationScope::Prod,
                operation_id: "blocking-operation".into(),
                kind: shared_types::UserAppOperationKind::Start,
                state: shared_types::UserAppOperationState::RecoveryRequired,
                step: "claimed".into(),
            },
        }
        .with_operation_id("accepted-operation".into());
        let converted: shared_types::AppError = error.into();
        let shared_types::AppError::Structured(detail) = converted else {
            panic!("structured conflict expected");
        };
        assert_eq!(detail.code, shared_types::ERR_OPERATION_IN_PROGRESS);
        assert_eq!(detail.operation_id.as_deref(), Some("accepted-operation"));
        let blocker = detail.blocker.expect("original blocker");
        assert_eq!(blocker.operation_id, "blocking-operation");
        assert_eq!(blocker.scope, shared_types::UserAppOperationScope::Prod);
    }
    #[test]
    fn credential_diagnostic_keeps_typed_cause_and_mutation_fence() {
        for mutation in [
            shared_types::CredentialMutationEvidence::NotAttempted,
            shared_types::CredentialMutationEvidence::Unknown,
            shared_types::CredentialMutationEvidence::AppliedButUnverified,
        ] {
            let error = AppOperationError::credential_command_error(
                shared_types::PgCommandError::transport(
                    if mutation == shared_types::CredentialMutationEvidence::NotAttempted {
                        shared_types::PgCommandMode::ReadOnly
                    } else {
                        shared_types::PgCommandMode::Write
                    },
                    shared_types::ERR_RUNTIME_TIMEOUT,
                    "safe transport summary",
                ),
                "database_password_write",
                mutation,
                "Password write completion was not observed",
            )
            .with_operation_id("original-password-operation".into());
            assert_eq!(
                error.requires_recovery(),
                mutation != shared_types::CredentialMutationEvidence::NotAttempted
            );
            let wake = error.wake_failure("later_observation");
            assert_eq!(wake.cause_code.as_ref(), shared_types::ERR_RUNTIME_TIMEOUT);
            assert_eq!(wake.stage.as_ref(), "database_password_write");
            assert_eq!(
                wake.operation_id.as_deref(),
                Some("original-password-operation")
            );
            let result = shared_types::AppError::from(error).into_http_result::<()>("en-US");
            assert_eq!(
                result.error_detail.expect("detail").reason_code,
                shared_types::ERR_RUNTIME_TIMEOUT
            );
            assert_eq!(
                result.operation_id.as_deref(),
                Some("original-password-operation")
            );
        }
    }
}
