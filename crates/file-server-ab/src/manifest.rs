//! route 覆盖清单（route-coverage.json）维护、用例依赖图与执行证据判定。

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::types::*;

/// Route execution requires a matching API request from BOTH sides. A paired
/// CaseResult does not prove that: `run_pair_specs` permits side-specific specs, so
/// each side's own method/route evidence is checked. Returns the sides that are
/// missing a match, so callers can name them.
pub(crate) fn evidence_missing_sides(
    evidence: &[RequestEvidence],
    case: &str,
    method: &str,
    template: &str,
) -> Vec<&'static str> {
    ["rust", "typescript"]
        .into_iter()
        .filter(|side| {
            !evidence.iter().any(|entry| {
                entry.case == case
                    && entry.side == *side
                    && entry.target == RequestTarget::Api
                    && entry.method == method
                    && route_template_matches(template, &entry.path)
            })
        })
        .collect()
}

/// Match a concrete request path against a route template whose `:name` segments match
/// any single segment and a trailing `*` matches the remaining segments.
pub(crate) fn route_template_matches(template: &str, path: &str) -> bool {
    let template_segments: Vec<_> = template.trim_start_matches('/').split('/').collect();
    let path_segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
    for (index, segment) in template_segments.iter().enumerate() {
        if *segment == "*" {
            return true;
        }
        if segment.starts_with(':') {
            if !path_segments
                .get(index)
                .is_some_and(|segment| !segment.is_empty())
            {
                return false;
            }
            continue;
        }
        if path_segments.get(index) != Some(segment) {
            return false;
        }
    }
    template_segments.len() == path_segments.len()
}

/// Coverage is derived from completed paired cases cross-checked against the API
/// requests that were actually sent, never the selected suite or case names alone.
/// Returns descriptions of routes whose claimed cases have no matching request.
pub(crate) fn update_route_execution(
    coverage: &mut Value,
    cases: &[CaseResult],
    evidence: &[RequestEvidence],
    suite: Suite,
) -> Result<Vec<String>> {
    let mut inconsistencies = Vec::new();
    let entries = coverage
        .get_mut("entries")
        .and_then(Value::as_array_mut)
        .context("route coverage entries missing")?;
    for entry in entries {
        let planned_all = entry
            .get("cases")
            .and_then(Value::as_array)
            .context("route coverage cases missing")?
            .clone();
        let method = entry
            .get("method")
            .and_then(Value::as_str)
            .context("route coverage entry is missing method")?
            .to_string();
        let route = entry
            .get("path")
            .and_then(Value::as_str)
            .context("route coverage entry is missing path")?
            .to_string();
        let appeared: Vec<&CaseResult> = planned_all
            .iter()
            .filter_map(|name| name.as_str())
            .filter_map(|name| cases.iter().find(|case| case.case == name))
            .collect();
        // A route counts as executed only when both sides issued a matching API
        // request; missing sides are reported per case by name.
        let (mut executed, mut unverified): (Vec<&CaseResult>, Vec<String>) =
            (Vec::new(), Vec::new());
        for case in &appeared {
            if case.blocked_by.is_some() {
                continue;
            }
            let missing = evidence_missing_sides(evidence, &case.case, &method, &route);
            if missing.is_empty() {
                executed.push(case);
            } else {
                unverified.push(format!(
                    "{} (missing {} side request)",
                    case.case,
                    missing.join("+")
                ));
            }
        }
        if !unverified.is_empty() {
            inconsistencies.push(format!(
                "{method} {route}: cases completed without a matching API request on both sides: {}",
                unverified.join("; ")
            ));
        }
        let blocked: Vec<&str> = appeared
            .iter()
            .filter(|case| case.blocked_by.is_some())
            .map(|case| case.case.as_str())
            .collect();
        let planned_this_run: Vec<String> = planned_all
            .iter()
            .filter_map(|name| name.as_str())
            .filter(|name| case_matches_suite(name, suite))
            .map(str::to_string)
            .collect();
        let executed_planned: Vec<&str> = executed
            .iter()
            .map(|case| case.case.as_str())
            .filter(|name| planned_this_run.iter().any(|planned| planned == name))
            .collect();
        let status = if !unverified.is_empty() {
            "unverified"
        } else if executed.is_empty() {
            if blocked.is_empty() {
                "not_run"
            } else {
                "blocked"
            }
        } else if executed.iter().any(|case| {
            case.differences
                .iter()
                .any(|d| d.kind == "transport_error" || d.kind == "assertion_failed")
        }) {
            "failed"
        } else if executed_planned.len() < planned_this_run.len() {
            "partial"
        } else {
            "completed"
        };
        // Completed is execution evidence cross-checked with request records, not a
        // claim that the two responses are equivalent.
        let object = entry
            .as_object_mut()
            .context("route coverage entry is not an object")?;
        object.insert("executed_cases".into(), json!(executed_planned));
        object.insert(
            "executed_in_this_run".into(),
            json!(!executed_planned.is_empty()),
        );
        object.insert("blocked_cases".into(), json!(blocked));
        object.insert("execution_status".into(), json!(status));
    }
    Ok(inconsistencies)
}

/// Whether a case name belongs to the selected suite. Shared by the initial planning
/// pass and the execution-derived coverage so both use one prefix rule.
pub(crate) fn case_matches_suite(case: &str, suite: Suite) -> bool {
    match suite {
        Suite::All => true,
        Suite::Core => !case.starts_with("git-") && !case.starts_with("build-"),
        Suite::Git => case.starts_with("git-"),
        Suite::Build => case.starts_with("build-"),
    }
}

/// A case blocks its dependents only on failures that invalidate the state they read:
/// failed assertions or transport errors. Cosmetic value differences do not block.
pub(crate) fn case_failed(case: &CaseResult) -> bool {
    case.differences
        .iter()
        .any(|d| d.kind == "transport_error" || d.kind == "assertion_failed")
}

pub(crate) fn find_blocker(cases: &[CaseResult], dependencies: &[&str]) -> Option<String> {
    dependencies.iter().find_map(|dependency| {
        cases
            .iter()
            .find(|case| {
                case.case.as_str() == *dependency
                    && (case_failed(case) || case.blocked_by.is_some())
            })
            .map(|case| case.case.clone())
    })
}

pub(crate) fn blocked_case(case: &str, blocker: &str) -> CaseResult {
    let difference = Difference {
        case: case.to_string(),
        path: "/blocked".into(),
        kind: "blocked".into(),
        rust_value: None,
        ts_value: None,
        accepted: false,
        reason: Some(format!("skipped because dependency `{blocker}` failed")),
    };
    CaseResult {
        case: case.to_string(),
        equal: false,
        compared: format!("not executed; blocked by failed dependency `{blocker}`"),
        normalized_paths: Vec::new(),
        normalized_headers: Vec::new(),
        rust_status: None,
        ts_status: None,
        differences: vec![difference],
        blocked_by: Some(blocker.to_string()),
    }
}

/// Data dependencies between scenarios. A case is only meaningful after its
/// dependencies produced the state it reads; this also covers fixture preparations
/// attached to a specific case. Unlisted cases are independent.
pub(crate) fn case_dependencies(case: &str) -> &'static [&'static str] {
    match case {
        // React template project chain
        "read-react-template-project"
        | "update-project-file"
        | "read-project-static-file-range" => &["create-react-template-project"],
        "read-project-static-file" => &["update-project-file"],
        "read-project-static-file-if-none-match" => &["read-project-static-file"],
        "read-vue-template-project" => &["create-vue-template-project"],
        // Computer session chains
        "computer-read-files-update-content" => &["computer-files-update-mixed-operations"],
        "computer-read-uploaded-single-binary" => &["computer-upload-file-binary"],
        "computer-read-uploaded-batch-binary" => &["computer-upload-files-mixed-content"],
        "fs-rename-preserve-spaces" => &["fs-mkdir-preserve-spaces"],
        // Skills workspaces (independent per workspace id)
        "computer-push-skills-v1-local-zip" | "computer-delete-workspace-owned-fixture" => {
            &["computer-create-workspace-v1-local-skills"]
        }
        "computer-push-skills-v2-local-zip" => &["computer-create-workspace-v2-local-config"],
        "computer-read-generated-file-utf8" => &["computer-generate-file-utf8"],
        "computer-read-imported-project-file" | "computer-import-preservation-contract" => {
            &["computer-import-project-local-zip"]
        }
        "computer-init-template-git-tree" | "computer-install-empty-typescript-project" => {
            &["computer-init-project-template-package-fixture"]
        }
        "computer-build-agent-package-synthetic" => &["computer-install-empty-typescript-project"],
        "computer-cleanup-build-artifacts" => &["computer-build-agent-package-synthetic"],
        // Project lifecycle chain
        "project-all-files-update-seed-obsolete" => &["project-fixture-lifecycle-create"],
        "project-all-files-update-replace" => &["project-all-files-update-seed-obsolete"],
        "project-all-files-update-removes-omitted-file" | "project-upload-single-file-bytes" => {
            &["project-all-files-update-replace"]
        }
        "project-upload-batch-mixed-bytes" => &["project-upload-single-file-bytes"],
        "project-upload-attachment-deterministic-name" => &["project-upload-batch-mixed-bytes"],
        "project-push-skills-local-zip" => &["project-upload-attachment-deterministic-name"],
        "project-backup-deprecated-under-git" => &["project-push-skills-local-zip"],
        "project-get-version-deprecated-under-git" => &["project-backup-deprecated-under-git"],
        "project-rollback-deprecated-under-git" => &["project-get-version-deprecated-under-git"],
        "project-copy-project-tree" => &["project-push-skills-local-zip"],
        "project-copy-git-history" => &["project-copy-project-tree"],
        "project-export-latest-semantic-zip" => &["project-push-skills-local-zip"],
        "project-upload-project-wrapper-zip" => &["project-fixture-upload-create"],
        "project-read-uploaded-project-file" => &["project-upload-project-wrapper-zip"],
        "project-delete-owned-fixture" => &["project-fixture-delete-create"],
        // Git main project chain; preparations for the worktree change are attached to
        // git-status-after-worktree-change, so later stages depend on it having run.
        "git-add-initial-files" => &["git-init"],
        "git-commit-initial" => &["git-add-initial-files"],
        "git-status-clean" | "git-read-head-file" | "git-create-branch" => &["git-commit-initial"],
        "git-list-branches-after-create" | "git-switch-main" => &["git-create-branch"],
        "git-create-tag" => &["git-switch-main"],
        "git-list-tags" => &["git-create-tag"],
        "git-delete-tag" => &["git-list-tags"],
        "git-status-after-worktree-change" => &["git-commit-initial"],
        "git-worktree-diff" => &["git-status-after-worktree-change"],
        "git-add-second-change" => &["git-worktree-diff"],
        "git-staged-diff" => &["git-add-second-change"],
        "git-unstage-new-file" => &["git-staged-diff"],
        "git-status-after-unstage" => &["git-unstage-new-file"],
        "git-restage-second-change" => &["git-status-after-unstage"],
        "git-commit-second" => &["git-restage-second-change"],
        "git-log" => &["git-commit-second"],
        // Seeded Git projects
        "git-branch-delete-seed-add-initial" => &["git-branch-delete-seed-init"],
        "git-branch-delete-seed-commit-initial" => &["git-branch-delete-seed-add-initial"],
        "git-delete-branch" => &["git-branch-delete-seed-commit-initial"],
        "git-list-branches-after-delete" => &["git-delete-branch"],
        "git-revert-seed-add-initial" => &["git-revert-seed-init"],
        "git-revert-seed-commit-initial" => &["git-revert-seed-add-initial"],
        "git-revert-seed-add-second" => &["git-revert-seed-commit-initial"],
        "git-revert-seed-commit-second" => &["git-revert-seed-add-second"],
        "git-revert-to-initial" => &["git-revert-seed-commit-second"],
        "git-reset-mixed-seed-add-initial" => &["git-reset-mixed-seed-init"],
        "git-reset-mixed-seed-commit-initial" => &["git-reset-mixed-seed-add-initial"],
        "git-reset-mixed-seed-add-second" => &["git-reset-mixed-seed-commit-initial"],
        "git-reset-mixed-seed-commit-second" => &["git-reset-mixed-seed-add-second"],
        "git-reset-mixed-to-first" => &["git-reset-mixed-seed-commit-second"],
        "git-reset-hard-seed-add-initial" => &["git-reset-hard-seed-init"],
        "git-reset-hard-seed-commit-initial" => &["git-reset-hard-seed-add-initial"],
        "git-reset-hard-seed-add-second" => &["git-reset-hard-seed-commit-initial"],
        "git-reset-hard-seed-commit-second" => &["git-reset-hard-seed-add-second"],
        "git-reset-hard-to-first" => &["git-reset-hard-seed-commit-second"],
        "git-reset-soft-seed-add-initial" => &["git-reset-soft-seed-init"],
        "git-reset-soft-seed-commit-initial" => &["git-reset-soft-seed-add-initial"],
        "git-reset-soft-seed-add-second" => &["git-reset-soft-seed-commit-initial"],
        "git-reset-soft-seed-commit-second" => &["git-reset-soft-seed-add-second"],
        "git-reset-soft-to-first" => &["git-reset-soft-seed-commit-second"],
        "git-checkout-seed-add-initial" => &["git-checkout-seed-init"],
        "git-checkout-seed-commit-initial" => &["git-checkout-seed-add-initial"],
        "git-checkout-head" => &["git-checkout-seed-commit-initial"],
        "git-status-after-checkout" => &["git-checkout-head"],
        "git-discard-seed-add-initial" => &["git-discard-seed-init"],
        "git-discard-seed-commit-initial" => &["git-discard-seed-add-initial"],
        "git-discard-all" => &["git-discard-seed-commit-initial"],
        "git-merge-conflict-seed-add-initial" => &["git-merge-conflict-seed-init"],
        "git-merge-conflict-seed-commit-initial" => &["git-merge-conflict-seed-add-initial"],
        "git-merge-conflict-create-feature" => &["git-merge-conflict-seed-commit-initial"],
        "git-merge-conflict-switch-feature" => &["git-merge-conflict-create-feature"],
        "git-merge-conflict-stage-feature" => &["git-merge-conflict-switch-feature"],
        "git-merge-conflict-commit-feature" => &["git-merge-conflict-stage-feature"],
        "git-merge-conflict-switch-main" => &["git-merge-conflict-commit-feature"],
        "git-merge-conflict-stage-main" => &["git-merge-conflict-switch-main"],
        "git-merge-conflict-commit-main" => &["git-merge-conflict-stage-main"],
        "git-status-after-merge-conflict" => &["git-merge-conflict-commit-main"],
        // Build lifecycle chains per template
        "build-react-production-build" => &["build-react-create-project"],
        "build-react-static-dist-index" | "build-react-start-dev" => {
            &["build-react-production-build"]
        }
        "build-react-dev-http-reachable" => &["build-react-start-dev"],
        "build-react-get-dev-log" | "build-react-port-pool-status" => {
            &["build-react-dev-http-reachable"]
        }
        "build-react-get-dev-log-page-2" => &["build-react-get-dev-log"],
        "build-react-log-cache-stats" => &["build-react-get-dev-log"],
        "build-react-clear-log-cache" => &["build-react-log-cache-stats"],
        "build-react-log-cache-stats-after-clear" => &["build-react-clear-log-cache"],
        "build-react-keep-alive" => &["build-react-port-pool-status"],
        "build-react-restart-dev" => &["build-react-keep-alive"],
        "build-react-restarted-dev-http-reachable" => &["build-react-restart-dev"],
        "build-react-stop-dev" => &["build-react-restarted-dev-http-reachable"],
        "build-react-stop-dev-port-unreachable" => &["build-react-stop-dev"],
        "build-react-list-after-stop" => &["build-react-stop-dev-port-unreachable"],
        "build-vue3-production-build" => &["build-vue3-create-project"],
        "build-vue3-static-dist-index" | "build-vue3-start-dev" => &["build-vue3-production-build"],
        "build-vue3-dev-http-reachable" => &["build-vue3-start-dev"],
        "build-vue3-keep-alive" => &["build-vue3-dev-http-reachable"],
        "build-vue3-restart-dev" => &["build-vue3-keep-alive"],
        "build-vue3-restarted-dev-http-reachable" => &["build-vue3-restart-dev"],
        "build-vue3-stop-dev" => &["build-vue3-restarted-dev-http-reachable"],
        "build-vue3-stop-dev-port-unreachable" => &["build-vue3-stop-dev"],
        "build-vue3-list-after-stop" => &["build-vue3-stop-dev-port-unreachable"],
        _ => &[],
    }
}
