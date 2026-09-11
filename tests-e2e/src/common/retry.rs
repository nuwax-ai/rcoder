//! Retry only identifiable transport/readiness transients, never validation or conflicts.
pub fn ensure_transient(status: reqwest::StatusCode, body: &serde_json::Value) -> bool {
    let code = body["code"].as_str();
    if matches!(
        code,
        Some("ERR_VALIDATION" | "ERR_CONFLICT" | "ERR_APP_ALREADY_EXISTS")
    ) {
        return false;
    }
    if matches!(status.as_u16(), 502..=504) {
        return true;
    }
    code == Some("ERR_CONTAINER_ERROR")
        && body["message"].as_str().is_some_and(|message| {
            let message = message.to_ascii_lowercase();
            [
                "connect",
                "not ready",
                "timed out",
                "deadline",
                "temporarily",
                "starting",
            ]
            .iter()
            .any(|text| message.contains(text))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conflicts_and_validation_are_never_retried() {
        for code in ["ERR_CONFLICT", "ERR_VALIDATION", "ERR_APP_ALREADY_EXISTS"] {
            assert!(!ensure_transient(
                reqwest::StatusCode::OK,
                &serde_json::json!({"code":code,"message":"connection starting"})
            ));
        }
        assert!(ensure_transient(
            reqwest::StatusCode::OK,
            &serde_json::json!({"code":"ERR_CONTAINER_ERROR","message":"connection refused"})
        ));
        assert!(!ensure_transient(
            reqwest::StatusCode::OK,
            &serde_json::json!({"code":"ERR_CONTAINER_ERROR","message":"image configuration invalid"})
        ));
    }
}
