//! Ordinary Computer registration errors shared by engine and HTTP boundaries.

use crate::{AppError, ErrorDetail, error_codes};

/// A request disagrees with an existing owner. Values stay in internal logs;
/// this type and its HTTP diagnostic expose only the mismatched dimension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ComputerOwnerMismatch {
    #[error("Computer project user owner does not match the request")]
    User,
    #[error("Computer project pod owner does not match the request")]
    Pod,
    #[error("Computer project service family does not match the request")]
    ServiceFamily,
}

impl ComputerOwnerMismatch {
    pub fn dimension(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Pod => "pod",
            Self::ServiceFamily => "service_family",
        }
    }

    pub fn reason_code(self) -> &'static str {
        match self {
            Self::User => error_codes::COMPUTER_OWNER_USER_MISMATCH,
            Self::Pod => error_codes::COMPUTER_OWNER_POD_MISMATCH,
            Self::ServiceFamily => error_codes::COMPUTER_OWNER_SERVICE_FAMILY_MISMATCH,
        }
    }

    fn message_key(self) -> &'static str {
        match self {
            Self::User => "error.computer_owner_user_mismatch",
            Self::Pod => "error.computer_owner_pod_mismatch",
            Self::ServiceFamily => "error.computer_owner_service_family_mismatch",
        }
    }
}

impl From<ComputerOwnerMismatch> for AppError {
    fn from(mismatch: ComputerOwnerMismatch) -> Self {
        Self::with_i18n_key(
            error_codes::ERR_COMPUTER_OWNER_CONFLICT,
            mismatch.message_key(),
        )
        .with_error_detail(
            ErrorDetail::new(
                mismatch.reason_code(),
                "computer.registration.before_dispatch",
                crate::get_i18n_message(mismatch.message_key(), crate::current_request_locale()),
            )
            .with_retryable(false),
        )
    }
}

/// Preserve typed registration errors through anyhow context. Infrastructure
/// failures keep their original 500/503 classification; text is never parsed.
pub fn computer_registration_app_error(error: anyhow::Error) -> AppError {
    match error.downcast::<ComputerOwnerMismatch>() {
        Ok(mismatch) => mismatch.into(),
        Err(error) => error.downcast::<AppError>().unwrap_or_else(AppError::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};

    #[tokio::test]
    async fn owner_conflict_http_contract_is_localized_and_never_authorizes_retry() {
        for mismatch in [
            ComputerOwnerMismatch::User,
            ComputerOwnerMismatch::Pod,
            ComputerOwnerMismatch::ServiceFamily,
        ] {
            for locale in crate::i18n::SUPPORTED_LOCALES {
                crate::scope_request_locale(locale, async {
                    let error = anyhow::Error::new(mismatch)
                        .context("private_registered_owner private_requested_owner");
                    let response = computer_registration_app_error(error).into_response();
                    assert_eq!(response.status(), StatusCode::CONFLICT);
                    let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
                    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(body["code"], error_codes::ERR_COMPUTER_OWNER_CONFLICT);
                    assert_eq!(body["success"], false);
                    assert_eq!(body["data"], serde_json::Value::Null);
                    assert_eq!(body["error_detail"]["reason_code"], mismatch.reason_code());
                    assert_eq!(
                        body["error_detail"]["stage"],
                        "computer.registration.before_dispatch"
                    );
                    assert_eq!(body["error_detail"]["retryable"], false);
                    assert_eq!(
                        body["message"],
                        crate::get_i18n_message(mismatch.message_key(), locale)
                    );
                    assert_eq!(body["error_detail"]["detail"], body["message"]);
                    assert_eq!(
                        body["error_detail"]["hint"],
                        crate::get_error_hint(mismatch.reason_code(), locale)
                    );
                    assert!(
                        body.get("tid").is_some(),
                        "shared trace field remains present"
                    );
                    assert!(body.get("operation_id").is_none());
                    let text = String::from_utf8(bytes.to_vec()).unwrap();
                    assert!(!text.contains("private_registered_owner"));
                    assert!(!text.contains("private_requested_owner"));
                })
                .await;
            }
        }
    }

    #[test]
    fn registration_errors_are_classified_by_type_and_preserve_infrastructure_failures() {
        for message in [
            "Computer project belongs to another registered owner",
            "SQL connection failed",
        ] {
            assert_eq!(
                computer_registration_app_error(anyhow::anyhow!(message)).status_code(),
                StatusCode::INTERNAL_SERVER_ERROR
            );
        }
        let unavailable = anyhow::Error::new(AppError::from_code(crate::ERR_SERVICE_UNAVAILABLE))
            .context("registration database unavailable");
        assert_eq!(
            computer_registration_app_error(unavailable).status_code(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn owner_conflict_dimensions_have_distinct_localized_messages_and_hints() {
        for locale in crate::i18n::SUPPORTED_LOCALES {
            let messages = [
                ComputerOwnerMismatch::User,
                ComputerOwnerMismatch::Pod,
                ComputerOwnerMismatch::ServiceFamily,
            ]
            .map(|mismatch| crate::get_i18n_message(mismatch.message_key(), locale));
            let hints = [
                ComputerOwnerMismatch::User,
                ComputerOwnerMismatch::Pod,
                ComputerOwnerMismatch::ServiceFamily,
            ]
            .map(|mismatch| crate::get_error_hint(mismatch.reason_code(), locale));
            for texts in [&messages, &hints] {
                for text in texts {
                    assert!(
                        !text.is_empty()
                            && !text.starts_with("error.")
                            && !text.starts_with("hint.")
                    );
                }
                assert_ne!(texts[0], texts[1]);
                assert_ne!(texts[0], texts[2]);
                assert_ne!(texts[1], texts[2]);
            }
        }
        for (mismatch, dimension) in [
            (ComputerOwnerMismatch::User, "运行账号"),
            (ComputerOwnerMismatch::Pod, "运行实例"),
            (ComputerOwnerMismatch::ServiceFamily, "服务类型"),
        ] {
            let message = crate::get_i18n_message(mismatch.message_key(), "zh-CN");
            assert!(message.contains(dimension) && message.contains("消息未发送"));
        }
        let generic = AppError::from_code(error_codes::ERR_COMPUTER_OWNER_CONFLICT)
            .into_http_result::<()>("zh-CN");
        assert_eq!(
            generic.message,
            crate::get_error_message(error_codes::ERR_COMPUTER_OWNER_CONFLICT, "zh-CN")
        );
        assert!(generic.error_detail.is_none());
    }
}
