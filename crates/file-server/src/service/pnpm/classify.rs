//! 将 pnpm 错误码和诊断文本归一化为稳定失败类别。

use super::error::FailureKind;
use super::types::InstallSummary;

pub(super) fn classify_failure(
    summary: &InstallSummary,
    fallback_output: &str,
) -> (FailureKind, Option<String>, String) {
    let code = summary.error_codes.last().cloned();
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
    let haystack = format!(
        "{} {message} {fallback_output}",
        code.as_deref().unwrap_or("")
    )
    .to_ascii_lowercase();
    (classify_text(&haystack), code, message)
}

fn classify_text(value: &str) -> FailureKind {
    if contains_any(
        value,
        &[
            "err_pnpm_fetch_401",
            "err_pnpm_fetch_403",
            "e401",
            "e403",
            "unauthorized",
            "forbidden",
        ],
    ) {
        FailureKind::RegistryAuth
    } else if contains_any(
        value,
        &[
            "err_pnpm_no_matching_version",
            "err_pnpm_fetch_404",
            "no matching version",
            "is not in the npm registry",
        ],
    ) {
        FailureKind::PackageNotFound
    } else if contains_any(
        value,
        &[
            "etimedout",
            "err_pnpm_meta_fetch_fail",
            "fetch timeout",
            "timed out",
        ],
    ) {
        FailureKind::NetworkTimeout
    } else if contains_any(
        value,
        &[
            "econnrefused",
            "econnreset",
            "enotfound",
            "enetunreach",
            "network is unreachable",
            "getaddrinfo",
        ],
    ) {
        FailureKind::NetworkUnavailable
    } else if contains_any(
        value,
        &[
            "err_pnpm_outdated_lockfile",
            "frozen_lockfile",
            "lockfile_breaking_change",
            "lockfile is not up to date",
        ],
    ) {
        FailureKind::LockfileMismatch
    } else if contains_any(
        value,
        &["err_pnpm_unsupported_engine", "unsupported engine"],
    ) {
        FailureKind::UnsupportedEngine
    } else if contains_any(
        value,
        &["err_pnpm_lifecycle", "elifecycle", "lifecycle script"],
    ) {
        FailureKind::LifecycleScript
    } else if contains_any(value, &["enospc", "no space left on device"]) {
        FailureKind::DiskFull
    } else if contains_any(
        value,
        &[
            "eacces",
            "eperm",
            "permission denied",
            "operation not permitted",
        ],
    ) {
        FailureKind::PermissionDenied
    } else if contains_any(
        value,
        &[
            "err_pnpm_tarb_bad_archive",
            "integrity check failed",
            "unexpected store",
            "store is corrupted",
        ],
    ) {
        FailureKind::StoreCorrupted
    } else {
        FailureKind::Unknown
    }
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
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
}
