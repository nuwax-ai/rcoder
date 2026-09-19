//! app_manager service 层错误类型。
//!
//! 从 models.rs 抽出——错误类型与数据模型职责不同（SRP）：service 抛出强类型错误，
//! handler 用 `impl From<AppOperationError> for AppError` 精确映射 HTTP。
//! models.rs 经 `pub use crate::error::{AppOperationError, AppResult};` re-export，
//! 故 `super::models::*` 与 crate 根 `app_manager::AppOperationError` 均可达（零调用方改动）。

use std::fmt;

use shared_types::error_codes::{
    ERR_APP_ALREADY_EXISTS, ERR_APP_NOT_FOUND, ERR_BACKEND_ERROR, ERR_CONFLICT,
    ERR_DEV_NOT_RUNNING, ERR_FILE_NOT_FOUND, ERR_INVALID_STATE, ERR_VALIDATION,
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
    /// Keep mutation evidence across service/HTTP error mapping. A failed TCP
    /// verification after ALTER must not be treated as a pre-mutation rejection.
    CredentialApplication {
        message: String,
        mutation: shared_types::CredentialMutationEvidence,
    },
    /// One remote HTTP request was explicitly rejected. This does not prove a
    /// multi-request operation had no earlier effects; callers retain that boundary.
    RuntimeRejected(shared_types::RuntimeRequestRejection),
    /// 乐观锁冲突（409 ERR_CONFLICT）—— expected_resource_version 不匹配
    Conflict(String),
    /// 受理被在途操作阻塞（409 ERR_CONFLICT）——携带结构化 blocker，
    /// 指名阻塞 scope/kind/state，随错误信封透出供调用方分支
    ConflictBlocked {
        message: String,
        blocker: shared_types::UserAppOperationBlocker,
    },
    HotDeployEnvChange(String),
}

impl AppOperationError {
    pub(crate) fn requires_recovery(&self) -> bool {
        matches!(
            self,
            Self::CredentialApplication {
                mutation: shared_types::CredentialMutationEvidence::Unknown
                    | shared_types::CredentialMutationEvidence::AppliedButUnverified,
                ..
            }
        )
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
            shared_types::AlignError::Command { .. } => {
                Self::CredentialApplication { message, mutation }
            }
        }
    }

    /// 业务错误码（ERR_* 常量）
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => ERR_APP_NOT_FOUND,
            Self::AlreadyExists(_) => ERR_APP_ALREADY_EXISTS,
            Self::InvalidState(_) => ERR_INVALID_STATE,
            Self::FileNotFound(_) => ERR_FILE_NOT_FOUND,
            Self::Validation(_) => ERR_VALIDATION,
            Self::DevNotRunning(_) => ERR_DEV_NOT_RUNNING,
            Self::Backend(_) => ERR_BACKEND_ERROR,
            Self::CredentialApplication { .. } if self.requires_recovery() => {
                shared_types::ERR_RECOVERY_REQUIRED
            }
            Self::CredentialApplication { .. } => ERR_BACKEND_ERROR,
            Self::RuntimeRejected(rejection) if rejection.status == 409 => ERR_CONFLICT,
            Self::RuntimeRejected(_) => ERR_BACKEND_ERROR,
            Self::Conflict(_) => ERR_CONFLICT,
            Self::ConflictBlocked { .. } => ERR_CONFLICT,
            Self::HotDeployEnvChange(_) => shared_types::error_codes::ERR_HOT_DEPLOY_ENV_CHANGE,
        }
    }

    /// 人读错误信息（含完整因果链，由 service 构造时拼入）
    pub fn message(&self) -> &str {
        match self {
            Self::RuntimeRejected(rejection) => &rejection.message,
            Self::ConflictBlocked { message, .. } | Self::CredentialApplication { message, .. } => {
                message
            }
            Self::NotFound(m)
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

impl std::error::Error for AppOperationError {}

impl From<shared_types::UserAppStoreError> for AppOperationError {
    fn from(error: shared_types::UserAppStoreError) -> Self {
        use shared_types::UserAppStoreError;
        match error {
            UserAppStoreError::OperationInProgress(blocker) => Self::ConflictBlocked {
                message: format!("Application operation in progress: {blocker}"),
                blocker,
            },
            UserAppStoreError::OwnershipConflict
            | UserAppStoreError::LifecycleConflict
            | UserAppStoreError::VersionConflict => Self::Conflict(error.to_string()),
            UserAppStoreError::NotFound => Self::NotFound(error.to_string()),
            UserAppStoreError::InvalidOperation(_) => Self::InvalidState(error.to_string()),
            UserAppStoreError::Storage(_) => Self::Backend(error.to_string()),
        }
    }
}

/// app service 操作返回类型
pub type AppResult<T> = Result<T, AppOperationError>;

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn credential_error_keeps_evidence_and_redacts_password() {
        for mutation in [
            shared_types::CredentialMutationEvidence::NotAttempted,
            shared_types::CredentialMutationEvidence::Unknown,
            shared_types::CredentialMutationEvidence::AppliedButUnverified,
        ] {
            let error = AppOperationError::credential_failure(
                shared_types::AlignError::Command {
                    stage: "credential step",
                    detail: "failed with privatepassword".into(),
                    mutation,
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
            assert!(
                matches!(error, AppOperationError::CredentialApplication { mutation: actual, .. } if actual == mutation)
            );
        }
    }
}
