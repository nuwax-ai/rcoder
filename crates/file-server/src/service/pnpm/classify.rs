//! 将 pnpm 错误码和诊断文本归一化为稳定失败类别。

use super::error::FailureKind;
use super::types::InstallSummary;

pub(super) fn classify_failure(
    summary: &InstallSummary,
    fallback_output: &str,
) -> (FailureKind, Option<String>, String) {
    // code 优先取 ndjson 事件里的 error_codes；缺失时兜底扫纯文本 `Error: ERR_PNPM_*`
    // 行（pnpm≥12 的 ignored-builds 等失败在 stderr 打纯文本、不走 ndjson 事件流——
    // 不扫则 code=None/Unknown，调用方（如 ignored-builds 自愈）无法精确门控）。
    let code = summary
        .error_codes
        .last()
        .cloned()
        .or_else(|| extract_plain_error_code(fallback_output));
    let message = summary
        .diagnostics
        .last()
        .cloned()
        .or_else(|| {
            fallback_output
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "pnpm exited without an error message".to_string());
    let kind = classify_tokens(code.as_deref(), &message, fallback_output);
    (kind, code, message)
}

/// 纯文本 `Error: ERR_PNPM_XXX` 行的码提取（倒序找最近一个）。
fn extract_plain_error_code(fallback_output: &str) -> Option<String> {
    fallback_output.lines().rev().find_map(|line| {
        let token = line
            .trim()
            .strip_prefix("Error: ")?
            .split_whitespace()
            .next()?;
        token.starts_with("ERR_PNPM_").then(|| token.to_string())
    })
}

/// FailureKind 归类 = 边界集中解析一次（业务层不得再匹配文案）。
/// 判据只有机器码，且按**整 token 等值**匹配（不子串）：
/// 1. `ERR_PNPM_*` code 表（ndjson error_codes / 纯文本提取），code token 优先；
/// 2. errno token 白名单（E401/ETIMEDOUT 等）。
///
/// 自由英文短语一律不参与分类（→ Unknown）：措辞随工具版本漂移，
/// 匹配文案即静默破坏分支逻辑。
fn classify_tokens(code: Option<&str>, message: &str, fallback_output: &str) -> FailureKind {
    let code = code.unwrap_or("");
    for raw in code
        .split_whitespace()
        .chain(message.split_whitespace())
        .chain(fallback_output.split_whitespace())
    {
        let token = raw
            .trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'))
            .to_ascii_lowercase();
        if let Some(kind) = kind_for_machine_code(&token) {
            return kind;
        }
    }
    FailureKind::Unknown
}

/// 机器码表（code 表驱动；勿添加自然语言短语）。
fn kind_for_machine_code(token: &str) -> Option<FailureKind> {
    let kind = match token {
        "err_pnpm_fetch_401" | "err_pnpm_fetch_403" | "e401" | "e403" => FailureKind::RegistryAuth,
        "err_pnpm_no_matching_version" | "err_pnpm_fetch_404" => FailureKind::PackageNotFound,
        "err_pnpm_meta_fetch_fail" | "etimedout" => FailureKind::NetworkTimeout,
        "econnrefused" | "econnreset" | "enotfound" | "enetunreach" => {
            FailureKind::NetworkUnavailable
        }
        "err_pnpm_outdated_lockfile" => FailureKind::LockfileMismatch,
        "err_pnpm_unsupported_engine" => FailureKind::UnsupportedEngine,
        "err_pnpm_lifecycle" | "elifecycle" => FailureKind::LifecycleScript,
        "enospc" => FailureKind::DiskFull,
        "eacces" | "eperm" => FailureKind::PermissionDenied,
        "err_pnpm_tarb_bad_archive" => FailureKind::StoreCorrupted,
        _ => return None,
    };
    Some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_failure_codes() {
        let cases = [
            ("ERR_PNPM_FETCH_401", FailureKind::RegistryAuth),
            ("ERR_PNPM_FETCH_404", FailureKind::PackageNotFound),
            (
                "ERR_PNPM_META_FETCH_FAIL ETIMEDOUT",
                FailureKind::NetworkTimeout,
            ),
            ("ECONNREFUSED", FailureKind::NetworkUnavailable),
            ("ERR_PNPM_OUTDATED_LOCKFILE", FailureKind::LockfileMismatch),
            (
                "ERR_PNPM_UNSUPPORTED_ENGINE",
                FailureKind::UnsupportedEngine,
            ),
            ("ELIFECYCLE", FailureKind::LifecycleScript),
            ("ENOSPC", FailureKind::DiskFull),
            ("EACCES", FailureKind::PermissionDenied),
        ];
        for (code, expected) in cases {
            let summary = InstallSummary {
                error_codes: vec![code.to_string()],
                ..InstallSummary::default()
            };
            assert_eq!(classify_failure(&summary, "").0, expected, "{code}");
        }
    }

    #[test]
    fn english_message_phrases_never_classify() {
        // T002 反例（修复前必挂）: 自由英文短语不得驱动分类——措辞随工具版本
        // 漂移, 匹配文案即静默破坏（含 ignored-builds 门控场景: 无 code 不触发自愈）。
        let cases = [
            "this package is unauthorized for the registry",
            "no matching version of left-pad",
            "permission denied while reading the store",
            "Ignored build scripts: run pnpm approve-builds",
        ];
        for fallback in cases {
            let summary = InstallSummary::default();
            assert_eq!(
                classify_failure(&summary, fallback).0,
                FailureKind::Unknown,
                "英文短语不得分类: {fallback}"
            );
        }
    }

    #[test]
    fn code_tokens_classify_despite_message_noise() {
        // code 表优先: message 是英文噪音时分类仍由机器码决定。
        let summary = InstallSummary {
            error_codes: vec!["ERR_PNPM_FETCH_401".to_string()],
            diagnostics: vec!["unauthorized, please login".to_string()],
            ..InstallSummary::default()
        };
        assert_eq!(
            classify_failure(&summary, "unauthorized").0,
            FailureKind::RegistryAuth
        );
        // 无 ERR_PNPM code 时 errno token 白名单仍是机器码判据
        let summary = InstallSummary::default();
        assert_eq!(
            classify_failure(&summary, "install failed\nETIMEDOUT").0,
            FailureKind::NetworkTimeout
        );
    }
}
