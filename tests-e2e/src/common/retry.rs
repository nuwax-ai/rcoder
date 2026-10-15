//! Retry only typed, proven transients. HTTP status and localized text are not evidence.
pub fn ensure_transient(_status: reqwest::StatusCode, body: &serde_json::Value) -> bool {
    let code = body["code"].as_str();
    if matches!(
        code,
        Some(
            "ERR_VALIDATION"
                | "ERR_CONFLICT"
                | "ERR_APP_ALREADY_EXISTS"
                | "ERR_INVALID_STATE"
                | "ERR_OPERATION_OUTCOME_UNKNOWN"
                | "ERR_RECOVERY_REQUIRED"
                | "ERR_STOP_PENDING"
                | "ERR_OPERATION_IN_PROGRESS"
        )
    ) {
        return false;
    }
    if let Some(retryable) = body["error_detail"]["retryable"].as_bool() {
        return retryable;
    }
    // Legacy, explicit pre-execution capacity/image failures only. Generic
    // backend/transport errors can conceal an accepted or unknown mutation.
    matches!(
        code,
        Some("ERR_IMAGE_PULL_FAILED" | "ERR_RESOURCE_EXHAUSTED")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_contract_unknown_write_and_explicit_denial_are_never_retried() {
        for body in [
            serde_json::json!({"code":"ERR_OPERATION_OUTCOME_UNKNOWN","message":"deadline exceeded"}),
            serde_json::json!({"code":"ERR_RUNTIME_TIMEOUT","error_detail":{"retryable":false}}),
        ] {
            assert!(!ensure_transient(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                &body
            ));
        }
    }

    #[test]
    fn error_contract_explicit_safe_retry_does_not_depend_on_message_language() {
        let body = serde_json::json!({
            "code":"ERR_CONTAINER_ADDRESS_NOT_READY",
            "message":"管理地址尚未就绪",
            "error_detail":{"retryable":true}
        });
        assert!(ensure_transient(reqwest::StatusCode::OK, &body));
    }
    #[test]
    fn conflicts_and_validation_are_never_retried() {
        for code in ["ERR_CONFLICT", "ERR_VALIDATION", "ERR_APP_ALREADY_EXISTS"] {
            assert!(!ensure_transient(
                reqwest::StatusCode::OK,
                &serde_json::json!({"code":code,"message":"connection starting"})
            ));
        }
        assert!(!ensure_transient(
            reqwest::StatusCode::OK,
            &serde_json::json!({"code":"ERR_CONTAINER_ERROR","message":"connection refused"})
        ));
        assert!(!ensure_transient(
            reqwest::StatusCode::GATEWAY_TIMEOUT,
            &serde_json::json!({"code":"ERR_BACKEND_ERROR","message":"starting"})
        ));
        assert!(!ensure_transient(
            reqwest::StatusCode::OK,
            &serde_json::json!({"code":"ERR_RECOVERY_REQUIRED","error_detail":{"retryable":true}})
        ));
        assert!(!ensure_transient(
            reqwest::StatusCode::OK,
            &serde_json::json!({"code":"ERR_CONTAINER_ERROR","message":"image configuration invalid"})
        ));
    }
}
