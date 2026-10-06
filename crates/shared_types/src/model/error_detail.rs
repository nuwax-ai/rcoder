//! Safe diagnostics shared by HTTP errors and streamed error payloads.
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Maximum number of Unicode characters exposed in one diagnostic string.
pub const MAX_ERROR_DETAIL_CHARS: usize = 4096;

/// Typed origin and retry evidence. None of these fields authorize execution.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct ErrorDetail {
    /// Stable reason supplied by the producer, never parsed from message text.
    pub reason_code: String,
    /// Original failing stage, distinct from a later diagnostic observation.
    pub stage: String,
    /// Bounded, sanitized original cause.
    pub detail: String,
    /// Repair guidance; default guidance uses the request language.
    pub hint: String,
    /// Safe retry with the same input and original operation identity.
    #[serde(default)]
    pub retryable: bool,
    /// A real retained task, when known. Never synthesized for correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// A real service/module identity, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
}

impl ErrorDetail {
    pub fn new(
        reason_code: impl Into<String>,
        stage: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let reason_code = reason_code.into();
        let hint = crate::error_codes::get_error_hint(&reason_code, crate::i18n::DEFAULT_LOCALE);
        Self {
            reason_code,
            stage: stage.into(),
            detail: sanitize_error_text(&detail.into()),
            hint,
            retryable: false,
            task_id: None,
            service_id: None,
        }
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = sanitize_error_text(&hint.into());
        self
    }

    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn with_service_id(mut self, service_id: impl Into<String>) -> Self {
        self.service_id = Some(service_id.into());
        self
    }

    /// Localize catalog guidance without translating third-party cause text.
    pub fn localized(&self, locale: &str) -> Self {
        let mut detail = self.clone();
        if crate::i18n::SUPPORTED_LOCALES.iter().any(|catalog_locale| {
            detail.hint == crate::error_codes::get_error_hint(&detail.reason_code, catalog_locale)
        }) {
            detail.hint = crate::error_codes::get_error_hint(&detail.reason_code, locale);
        }
        detail.detail = sanitize_error_text(&detail.detail);
        detail.hint = sanitize_error_text(&detail.hint);
        detail
    }
}

impl Serialize for ErrorDetail {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct SafeDetail<'a> {
            reason_code: &'a str,
            stage: &'a str,
            detail: String,
            hint: String,
            retryable: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            task_id: &'a Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            service_id: &'a Option<String>,
        }
        SafeDetail {
            reason_code: &self.reason_code,
            stage: &self.stage,
            detail: sanitize_error_text(&self.detail),
            hint: sanitize_error_text(&self.hint),
            retryable: self.retryable,
            task_id: &self.task_id,
            service_id: &self.service_id,
        }
        .serialize(serializer)
    }
}

type RedactionPatterns = Result<Vec<(Regex, &'static str)>, regex::Error>;

static REDACTION_PATTERNS: LazyLock<RedactionPatterns> = LazyLock::new(|| {
    [
            (r"(?i)([a-z][a-z0-9+.-]*://)[^\s/@]+(?::[^\s/@]*)?@", "$1[REDACTED]@"),
            (
                r#"(?i)((?:["']?)(?:[a-z0-9_]*(?:password|passwd|token|secret|api[_-]?key|access[_-]?key|authorization)[a-z0-9_]*)(?:["']?)\s*[:=]\s*)(?:"(?:\\.|[^"\\])*"|'(?:[^']|'')*'|(?:bearer|basic)\s+[^\s,;]+|[^\s,;]+)"#,
                "$1[REDACTED]",
            ),
            (r#"(?i)(\bpassword\s+)(?:'(?:[^']|'')*'|"(?:\\.|[^"\\])*")"#, "$1[REDACTED]"),
            (r"(?i)(\b(?:bearer|basic)\s+)[A-Za-z0-9+/_.~=-]+", "$1[REDACTED]"),
        ]
        .into_iter()
        .map(|(pattern, replacement)| Regex::new(pattern).map(|regex| (regex, replacement)))
        .collect()
});

/// Redact credential encodings without truncating application command output.
/// Credential-bearing paths must also redact their captured runtime secrets.
pub fn redact_error_text(text: &str) -> String {
    let patterns = match REDACTION_PATTERNS.as_ref() {
        Ok(patterns) => patterns,
        Err(error) => {
            tracing::error!(%error, "Diagnostic redaction configuration is invalid");
            return "Diagnostic unavailable: redaction configuration is invalid".into();
        }
    };
    let mut safe = text.to_owned();
    for (pattern, replacement) in patterns {
        safe = pattern.replace_all(&safe, *replacement).into_owned();
    }
    safe
}

/// Redact common credential encodings before logs or responses, then bound text.
/// Credential-bearing execution paths must still supply safe summaries: arbitrary
/// shell escaping cannot reliably be reversed by a generic text scrubber.
pub fn sanitize_error_text(text: &str) -> String {
    let mut safe = redact_error_text(text);
    if safe.chars().count() > MAX_ERROR_DETAIL_CHARS {
        safe = safe.chars().take(MAX_ERROR_DETAIL_CHARS).collect();
        safe.push_str("… [truncated]");
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_output_redaction_preserves_the_entire_long_diagnostic() {
        let text = format!(
            "{} postgresql://user:secret@localhost/db end-marker",
            "x".repeat(MAX_ERROR_DETAIL_CHARS * 2)
        );
        let safe = redact_error_text(&text);
        assert!(safe.ends_with("end-marker"));
        assert!(safe.len() > MAX_ERROR_DETAIL_CHARS);
        assert!(!safe.contains("secret"));
        assert!(sanitize_error_text(&text).ends_with("… [truncated]"));
    }

    #[test]
    fn diagnostic_redaction_covers_uri_json_headers_and_sql() {
        let text = r#"postgres://admin:uri_marker@db/app {"password":"json_marker"} POSTGRES_PASSWORD='env_marker' Authorization: Bearer token_marker; ALTER ROLE u PASSWORD 'sql_marker'; /workspace/服务"#;
        let safe = sanitize_error_text(text);
        for secret in [
            "uri_marker",
            "json_marker",
            "env_marker",
            "token_marker",
            "sql_marker",
        ] {
            assert!(!safe.contains(secret), "{safe}");
        }
        assert!(safe.contains("db/app"));
        assert!(safe.contains("/workspace/服务"));
        assert_eq!(
            sanitize_error_text("password update outcome is unknown"),
            "password update outcome is unknown"
        );
    }

    #[test]
    fn diagnostics_bound_unicode_and_default_to_no_retry() {
        let detail = ErrorDetail::new("write_unknown", "apply", "服".repeat(5000));
        assert!(!detail.retryable);
        assert!(detail.detail.starts_with('服'));
        assert!(detail.detail.ends_with("[truncated]"));
        assert!(detail.task_id.is_none());
        assert!(detail.service_id.is_none());
    }

    #[tokio::test]
    async fn diagnostic_guidance_uses_each_request_locale() {
        let (zh, en) = tokio::join!(
            crate::scope_request_locale("zh-CN", async {
                ErrorDetail::new("ERR_OPERATION_OUTCOME_UNKNOWN", "write", "safe cause")
                    .localized(crate::current_request_locale())
            }),
            crate::scope_request_locale("en-US", async {
                ErrorDetail::new("ERR_OPERATION_OUTCOME_UNKNOWN", "write", "safe cause")
                    .localized(crate::current_request_locale())
            }),
        );
        assert_ne!(zh.hint, en.hint);
        assert_eq!(zh.detail, en.detail);
        assert_eq!(zh.localized("en-US").hint, en.hint);
        assert!(!zh.retryable && !en.retryable);
    }
}
