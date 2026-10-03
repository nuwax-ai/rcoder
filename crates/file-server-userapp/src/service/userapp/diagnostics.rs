//! Diagnostic construction and admission errors. No source text classification.
use std::path::Path;

use file_server::error::AppError;
use shared_types::{
    UserAppDiagnostic, UserAppDiagnosticCode as Code, UserAppDiagnosticPhase as Phase,
    UserAppDiagnosticScope as Scope, UserAppRepairTarget as Target,
};

pub(crate) const MAX_DIAGNOSTICS: usize = 32;
const MAX_TEXT_CHARS: usize = 2048;

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut characters = text.chars();
    let mut result: String = characters.by_ref().take(max_chars).collect();
    if max_chars > 0 && characters.next().is_some() {
        let _ = result.pop();
        result.push('…');
    }
    result
}

pub(crate) fn bounded(text: &str) -> String {
    truncate_text(
        &file_server::service::dev_server::log::sanitize_sensitive_paths(text),
        MAX_TEXT_CHARS,
    )
}

pub(crate) fn diagnostic(
    code: Code,
    phase: Phase,
    target: Target,
    ws: Option<&Path>,
    message: impl AsRef<str>,
    hint: impl AsRef<str>,
) -> UserAppDiagnostic {
    UserAppDiagnostic {
        code,
        phase,
        repair_target: target,
        scope: Scope::Task,
        workspace_root: ws.map(|p| truncate_text(&p.display().to_string(), MAX_TEXT_CHARS)),
        detected_workspace_root: None,
        file: None,
        field: None,
        service_id: None,
        message: bounded(message.as_ref()),
        hint: bounded(hint.as_ref()),
    }
}

pub(crate) fn from_manifest(
    ws: &Path,
    item: shared_types::ManifestDiagnostic,
) -> UserAppDiagnostic {
    let (code, target) = match item.kind {
        shared_types::DiagnosticKind::Parse => (Code::ManifestParse, Target::Project),
        shared_types::DiagnosticKind::Validation => (Code::ManifestValidation, Target::Project),
        shared_types::DiagnosticKind::Io => (Code::WorkspaceIo, Target::Platform),
    };
    let mut result = diagnostic(
        code,
        Phase::Precheck,
        target,
        Some(ws),
        &item.issue.message,
        item.issue.hint.as_deref().unwrap_or("检查配置文件后重试。"),
    );
    result.file = item.issue.file.map(|v| bounded(&v));
    result.field = item.issue.field.map(|v| bounded(&v));
    result.service_id = item.issue.service.map(|v| bounded(&v));
    if result.service_id.is_some() {
        result.scope = Scope::Service;
    }
    result
}

pub(crate) fn log_line(diagnostic: &UserAppDiagnostic) -> String {
    let phase = match diagnostic.phase {
        Phase::Precheck => "源码预检查",
        Phase::OwnerPreflight => "运行所有者预检查",
        Phase::Admission => "任务受理",
        Phase::Build => "应用构建",
        Phase::Start => "应用服务启动",
    };
    let location = [diagnostic.file.as_deref(), diagnostic.field.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let line = if location.is_empty() {
        format!(
            "{phase}失败：{}；修复建议：{}",
            diagnostic.message, diagnostic.hint
        )
    } else {
        format!(
            "{phase}失败：{}；位置：{location}；修复建议：{}",
            diagnostic.message, diagnostic.hint
        )
    };
    truncate_text(&line, MAX_TEXT_CHARS * 2)
}

/// A worker may reject submission after registration. Retain that original ID.
#[derive(Debug)]
pub struct TaskSubmissionError {
    pub error: Box<AppError>,
    pub task_id: Option<String>,
    pub diagnostics: Vec<UserAppDiagnostic>,
}
impl From<AppError> for TaskSubmissionError {
    fn from(error: AppError) -> Self {
        Self {
            error: Box::new(error),
            task_id: None,
            diagnostics: Vec::new(),
        }
    }
}
impl std::fmt::Display for TaskSubmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for TaskSubmissionError {}

impl TaskSubmissionError {
    pub(crate) fn capacity(error: super::tasks::BuildTaskStoreError) -> Self {
        let item = diagnostic(
            Code::TaskCapacity,
            Phase::Admission,
            Target::Platform,
            None,
            error.to_string(),
            "等待现有任务结束后重试；本次未注册可查询的任务。",
        );
        Self {
            error: Box::new(AppError::business(error.to_string())),
            task_id: None,
            diagnostics: vec![item],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_text_marks_truncation_without_cutting_unicode_or_exceeding_limit() {
        for character in ['a', '中', '🙂'] {
            let exact = character.to_string().repeat(MAX_TEXT_CHARS);
            assert_eq!(bounded(&exact), exact, "exactly fitting text stays intact");
            let shortened = bounded(&format!("{exact}{character}"));
            assert_eq!(shortened.chars().count(), MAX_TEXT_CHARS);
            assert!(shortened.ends_with('…'));
            assert!(
                shortened
                    .chars()
                    .take(MAX_TEXT_CHARS - 1)
                    .all(|actual| actual == character)
            );
        }
        let item = diagnostic(
            Code::BuildFailed,
            Phase::Build,
            Target::Project,
            None,
            "中".repeat(MAX_TEXT_CHARS + 1),
            "修".repeat(MAX_TEXT_CHARS + 1),
        );
        let line = log_line(&item);
        assert_eq!(line.chars().count(), MAX_TEXT_CHARS * 2);
        assert!(
            line.ends_with('…'),
            "combined log truncation is visible too"
        );
    }

    #[test]
    fn boxed_submission_error_preserves_typed_recovery_details() {
        let recovery = serde_json::json!({"operation_id":"original-operation"});
        let error = TaskSubmissionError::from(AppError::RuntimeRecovery(
            "original failure".into(),
            recovery.clone(),
        ));
        let source = error.error.as_ref();
        assert!(
            matches!(source, AppError::RuntimeRecovery(message, data) if message == "original failure" && data == &recovery)
        );
        assert_eq!(error.to_string(), "original failure");
    }
}
