use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::{Client, Method, StatusCode, header::HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const BODY_CAPTURE_LIMIT: usize = 2 * 1024 * 1024;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(90);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CASE_USER: &str = "file-server-ab-user";
const CASE_CID: &str = "file-server-ab-session";
const AB_MULTIPART_BOUNDARY: &str = "----file-server-ab-boundary-6d86f5";
const AB_PACKAGE_CID: &str = "file-server-ab-package-session";
const AB_IMPORT_CID: &str = "file-server-ab-import-session";
const AB_SKILLS_V1_CID: &str = "file-server-ab-skills-v1-session";
const AB_SKILLS_V2_CID: &str = "file-server-ab-skills-v2-session";
const AB_LOG_CID: &str = "file-server-ab-log-session";
const ZIP_ENTRY_SIZE_LIMIT: u64 = 64 * 1024 * 1024;
const ZIP_TOTAL_SIZE_LIMIT: u64 = 128 * 1024 * 1024;
const ZIP_ENTRY_COUNT_LIMIT: usize = 20_000;

#[derive(Debug, Parser)]
#[command(
    name = "file-server-ab",
    about = "Black-box A/B comparison for file-server implementations"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check both HTTP services without mutating workspaces.
    Doctor {
        #[arg(long)]
        rust_url: String,
        #[arg(long)]
        ts_url: String,
    },
    /// Run the deterministic core suite and write a complete evidence bundle.
    Run(Box<RunOptions>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Suite {
    Core,
    Git,
    Build,
    All,
}

impl Suite {
    fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Git => "git",
            Self::Build => "build",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Serialize)]
struct Manifest {
    run_id: String,
    suite: String,
    started_at: String,
    configuration_profile: String,
    rust_source: String,
    ts_source: String,
    rust_image: String,
    ts_image: String,
    docker_builder: String,
    rust_node_version: String,
    ts_node_version: String,
    rust_runtime_architecture: String,
    ts_runtime_architecture: String,
    rust_pnpm_version: String,
    ts_pnpm_version: String,
    pnpm_registry: String,
    pnpm_network_concurrency: String,
    pnpm_rust_store_volume: String,
    pnpm_rust_metadata_cache_volume: String,
    pnpm_typescript_store_volume: String,
    pnpm_typescript_metadata_cache_volume: String,
    rust_git_version: String,
    ts_git_version: String,
    fixtures: BTreeMap<String, String>,
    rules_sha256: String,
    route_coverage_sha256: String,
    route_coverage_baseline_matches: bool,
    endpoints: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RulesFile {
    schema_version: u32,
    rules: Vec<DifferenceRule>,
}

#[derive(Debug, Deserialize)]
struct DifferenceRule {
    case: String,
    path: String,
    kind: String,
    expected_rust: Value,
    expected_typescript: Value,
    reason: String,
    reviewed_by: String,
    expires_on: String,
}

#[derive(Debug, Serialize)]
struct RequestLine {
    request_id: String,
    case: String,
    side: String,
    method: String,
    url: String,
    request_headers: BTreeMap<String, String>,
    request_body_bytes: usize,
    request_body_sha256: String,
    request_body_file: Option<String>,
    status: Option<u16>,
    response_headers: BTreeMap<String, String>,
    response_body_bytes: Option<usize>,
    response_body_sha256: Option<String>,
    response_body_file: Option<String>,
    request_body_truncated: bool,
    response_body_truncated: bool,
    headers_elapsed_ms: Option<u128>,
    elapsed_ms: u128,
    transport_error: Option<String>,
}

/// Written before a request is sent, so an interrupted run still shows requests
/// that were started but never produced a result line.
#[derive(Debug, Serialize)]
struct RequestStartLine {
    request_id: String,
    case: String,
    side: String,
    method: String,
    url: String,
    started_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Difference {
    case: String,
    path: String,
    kind: String,
    rust_value: Option<Value>,
    ts_value: Option<Value>,
    accepted: bool,
    reason: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct CaseResult {
    case: String,
    equal: bool,
    compared: String,
    normalized_paths: Vec<String>,
    normalized_headers: Vec<String>,
    rust_status: Option<u16>,
    ts_status: Option<u16>,
    differences: Vec<Difference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked_by: Option<String>,
}

/// Where a recorded request was sent. Only API requests can verify route coverage;
/// dev-server probes reuse paths such as `/` that belong to the API service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestTarget {
    Api,
    DevServer,
}

/// In-memory mirror of `requests.jsonl` used to cross-check route coverage against
/// the requests that were actually sent, including requests whose paired case never
/// completed because the run exited in between.
#[derive(Debug)]
struct RequestEvidence {
    case: String,
    side: String,
    method: String,
    path: String,
    target: RequestTarget,
}

struct RequestJournal {
    report_dir: PathBuf,
    file: fs::File,
    evidence: Vec<RequestEvidence>,
    started: Vec<(String, String)>,
}

impl RequestJournal {
    fn new(report_dir: PathBuf) -> Result<Self> {
        let file = fs::File::create(report_dir.join("requests.jsonl"))
            .with_context(|| format!("create {}", report_dir.join("requests.jsonl").display()))?;
        Ok(Self {
            report_dir,
            file,
            evidence: Vec::new(),
            started: Vec::new(),
        })
    }

    fn begin(&mut self, start: &RequestStartLine) -> Result<()> {
        append_jsonl(&mut self.file, start)?;
        self.started.push((start.case.clone(), start.side.clone()));
        Ok(())
    }

    fn append(
        &mut self,
        record: &RequestLine,
        request_path: &str,
        target: RequestTarget,
    ) -> Result<()> {
        append_jsonl(&mut self.file, record)?;
        let path_only = request_path
            .split('?')
            .next()
            .unwrap_or_default()
            .to_string();
        self.evidence.push(RequestEvidence {
            case: record.case.clone(),
            side: record.side.clone(),
            method: record.method.clone(),
            path: path_only,
            target,
        });
        Ok(())
    }
}

/// Route execution requires a matching API request from BOTH sides. A paired
/// CaseResult does not prove that: `run_pair_specs` permits side-specific specs, so
/// each side's own method/route evidence is checked. Returns the sides that are
/// missing a match, so callers can name them.
fn evidence_missing_sides(
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
fn route_template_matches(template: &str, path: &str) -> bool {
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

#[derive(Debug, Serialize, PartialEq, Eq)]
struct SnapshotEntry {
    path: String,
    kind: String,
    sha256: Option<String>,
    size_bytes: Option<u64>,
    mode: Option<String>,
    symlink_target: Option<String>,
}

#[derive(Debug, Serialize)]
struct GitStateSnapshot {
    repository_exists: bool,
    head_reference: Option<String>,
    head_tree: Option<String>,
    refs: BTreeMap<String, String>,
    index_entries: Vec<String>,
    status_entries: Vec<String>,
    capture_errors: Vec<String>,
}

#[derive(Debug)]
struct GitScenario {
    name: String,
    project_id: String,
    spec: RequestSpec,
    preparation: Option<GitPreparation>,
}

#[derive(Debug, Clone, Copy)]
enum GitPreparation {
    MainWorktreeChange,
    SecondCommitChange,
    CheckoutDirty,
    DiscardDirty,
    ResetHardDirty,
    CreateDeleteBranch,
    MergeConflictFeatureChange,
    MergeConflictMainChange,
    CreateMergeConflict,
}

const GIT_PROJECT_MAIN: &str = "file-server-ab-git";
const GIT_PROJECT_BRANCH_DELETE: &str = "file-server-ab-git-branch-delete";
const GIT_PROJECT_REVERT: &str = "file-server-ab-git-revert";
const GIT_PROJECT_RESET_MIXED: &str = "file-server-ab-git-reset-mixed";
const GIT_PROJECT_RESET_HARD: &str = "file-server-ab-git-reset-hard";
const GIT_PROJECT_RESET_SOFT: &str = "file-server-ab-git-reset-soft";
const GIT_PROJECT_CHECKOUT: &str = "file-server-ab-git-checkout";
const GIT_PROJECT_DISCARD: &str = "file-server-ab-git-discard";
const GIT_PROJECT_MERGE_CONFLICT: &str = "file-server-ab-git-merge-conflict";
const GIT_PROJECT_IDS: &[&str] = &[
    GIT_PROJECT_MAIN,
    GIT_PROJECT_BRANCH_DELETE,
    GIT_PROJECT_REVERT,
    GIT_PROJECT_RESET_MIXED,
    GIT_PROJECT_RESET_HARD,
    GIT_PROJECT_RESET_SOFT,
    GIT_PROJECT_CHECKOUT,
    GIT_PROJECT_DISCARD,
    GIT_PROJECT_MERGE_CONFLICT,
];

#[derive(Debug, Serialize)]
struct RunDiff {
    cases: Vec<CaseResult>,
    initial_state_differences: Vec<Difference>,
    state_differences: Vec<Difference>,
    git_state_differences: Vec<Difference>,
    summary: Summary,
}

#[derive(Debug, Serialize, Clone)]
struct Summary {
    comparisons: usize,
    equal: usize,
    expected_differences: usize,
    unclassified_differences: usize,
    transport_errors: usize,
    environment_errors: usize,
    environment_blocked: bool,
    precondition_errors: usize,
    blocked_cases: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    incomplete_reason: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct DifferenceGroup {
    scope: String,
    path: String,
    kind: String,
    accepted: bool,
    occurrences: usize,
    cases: BTreeSet<String>,
}

type DifferenceGroupKey = (String, String, String, bool);
type DifferenceGroupCounts = (usize, BTreeSet<String>);

#[derive(Debug)]
struct Exchange {
    status: Option<StatusCode>,
    headers: HeaderMap,
    body: Vec<u8>,
    transport_error: Option<String>,
}

#[derive(Debug)]
struct RequestSpec {
    method: Method,
    path: String,
    body: Vec<u8>,
    content_type: Option<&'static str>,
    headers: BTreeMap<String, String>,
    health_probe: bool,
    expected_status: ExpectedStatus,
    normalized_paths: Vec<String>,
    normalized_headers: Vec<String>,
    timeout: Duration,
}

#[derive(Debug, Clone, Copy)]
enum ExpectedStatus {
    Success2xx,
    ClientError4xx,
    PartialContent206,
    NotModified304,
}

impl ExpectedStatus {
    fn matches(self, status: Option<StatusCode>) -> bool {
        status.is_some_and(|status| match self {
            Self::Success2xx => status.is_success(),
            Self::ClientError4xx => status.is_client_error(),
            Self::PartialContent206 => status == StatusCode::PARTIAL_CONTENT,
            Self::NotModified304 => status == StatusCode::NOT_MODIFIED,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::Success2xx => "2xx",
            Self::ClientError4xx => "4xx",
            Self::PartialContent206 => "206",
            Self::NotModified304 => "304",
        }
    }
}

#[derive(Debug, Args)]
struct RunOptions {
    #[arg(long, value_enum, default_value_t = Suite::Core)]
    suite: Suite,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long)]
    rules: PathBuf,
    #[arg(long)]
    route_coverage: PathBuf,
    #[arg(long)]
    rust_url: String,
    #[arg(long)]
    ts_url: String,
    #[arg(long, default_value = "")]
    rust_dev_url: String,
    #[arg(long, default_value = "")]
    ts_dev_url: String,
    #[arg(long)]
    rust_root: PathBuf,
    #[arg(long)]
    ts_root: PathBuf,
    #[arg(long)]
    fixtures: PathBuf,
    #[arg(long)]
    report_root: PathBuf,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Doctor { rust_url, ts_url } => doctor(&rust_url, &ts_url).await,
        Command::Run(options) => run_suite(*options).await,
    };
    if let Err(error) = result {
        eprintln!("file-server-ab: {error:#}");
        std::process::exit(1);
    }
}

async fn doctor(rust_url: &str, ts_url: &str) -> Result<()> {
    let client = client()?;
    for (side, base) in [("rust", rust_url), ("ts", ts_url)] {
        let response = client
            .get(endpoint(base, "/health"))
            .timeout(HEALTH_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("connect to {side} file-server at {base}"))?;
        if !response.status().is_success() {
            bail!("{side} health returned HTTP {}", response.status());
        }
        println!("{side}: HTTP {}", response.status());
    }
    Ok(())
}

/// Coverage is derived from completed paired cases cross-checked against the API
/// requests that were actually sent, never the selected suite or case names alone.
/// Returns descriptions of routes whose claimed cases have no matching request.
fn update_route_execution(
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
fn case_matches_suite(case: &str, suite: Suite) -> bool {
    match suite {
        Suite::All => true,
        Suite::Core => !case.starts_with("git-") && !case.starts_with("build-"),
        Suite::Git => case.starts_with("git-"),
        Suite::Build => case.starts_with("build-"),
    }
}

/// A case blocks its dependents only on failures that invalidate the state they read:
/// failed assertions or transport errors. Cosmetic value differences do not block.
fn case_failed(case: &CaseResult) -> bool {
    case.differences
        .iter()
        .any(|d| d.kind == "transport_error" || d.kind == "assertion_failed")
}

fn find_blocker(cases: &[CaseResult], dependencies: &[&str]) -> Option<String> {
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

fn blocked_case(case: &str, blocker: &str) -> CaseResult {
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
fn case_dependencies(case: &str) -> &'static [&'static str] {
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

/// Accumulates completed cases and rewrites the route-coverage evidence after every
/// case, so an interrupted run still leaves an accurate partial report on disk.
struct Recorder {
    report_dir: PathBuf,
    suite: Suite,
    route_coverage: Value,
    cases: Vec<CaseResult>,
    cases_file: fs::File,
}

impl Recorder {
    fn new(report_dir: PathBuf, suite: Suite, route_coverage: Value) -> Result<Self> {
        let cases_file = fs::File::create(report_dir.join("cases.jsonl"))
            .with_context(|| format!("create {}", report_dir.join("cases.jsonl").display()))?;
        Ok(Self {
            report_dir,
            suite,
            route_coverage,
            cases: Vec::new(),
            cases_file,
        })
    }

    fn record(&mut self, journal: &RequestJournal, case: CaseResult) -> Result<()> {
        append_jsonl(&mut self.cases_file, &case).context("append incremental case record")?;
        self.cases.push(case);
        update_route_execution(
            &mut self.route_coverage,
            &self.cases,
            &journal.evidence,
            self.suite,
        )
        .context("update route coverage from executed cases")?;
        write_json(
            &self.report_dir.join("route-coverage.json"),
            &self.route_coverage,
        )
    }

    /// Persist a best-effort partial report when the run exits before completion.
    /// Failures while writing the partial report are reported on stderr only; the
    /// original error must still propagate to the caller.
    fn write_incomplete(
        &mut self,
        journal: &RequestJournal,
        run_id: &str,
        rules: &RulesFile,
        rust_root: &Path,
        ts_root: &Path,
        reason: &str,
    ) {
        // Distinguish requests that were started from requests that produced a
        // result line; a started-but-missing result means the run was interrupted
        // while waiting for that request.
        let mut attempted: BTreeMap<String, BTreeMap<String, (u64, u64)>> = BTreeMap::new();
        let mut bump = |case: &str, side: &str, slot: usize| {
            let entry = attempted
                .entry(case.to_string())
                .or_default()
                .entry(side.to_string())
                .or_insert((0, 0));
            if slot == 0 {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        };
        for (case, side) in &journal.started {
            bump(case, side, 0);
        }
        for entry in &journal.evidence {
            bump(&entry.case, &entry.side, 1);
        }
        if let Err(error) = write_json(&self.report_dir.join("attempts.json"), &attempted) {
            eprintln!("file-server-ab: could not write attempts.json: {error:#}");
        }
        let mut state_differences = Vec::new();
        match snapshot_roots(rust_root).and_then(|rust| {
            let ts = snapshot_roots(ts_root)?;
            write_json(&self.report_dir.join("state/rust.json"), &rust)?;
            write_json(&self.report_dir.join("state/typescript.json"), &ts)?;
            compare_snapshots(&rust, &ts)
        }) {
            Ok(differences) => state_differences = differences,
            Err(error) => {
                eprintln!("file-server-ab: could not capture final workspace state: {error:#}")
            }
        }
        for case in &mut self.cases {
            apply_rules(&mut case.differences, rules);
            case.equal = case.differences.is_empty();
        }
        apply_rules(&mut state_differences, rules);
        let equal = self.cases.iter().filter(|case| case.equal).count();
        let unclassified = self
            .cases
            .iter()
            .flat_map(|case| &case.differences)
            .filter(|difference| {
                !difference.accepted
                    && !matches!(difference.kind.as_str(), "transport_error" | "blocked")
            })
            .count()
            + state_differences
                .iter()
                .filter(|difference| !difference.accepted)
                .count();
        let diff = RunDiff {
            summary: Summary {
                comparisons: self.cases.len() + 2,
                equal,
                expected_differences: self
                    .cases
                    .iter()
                    .flat_map(|case| &case.differences)
                    .filter(|difference| difference.accepted)
                    .count(),
                unclassified_differences: unclassified,
                transport_errors: self
                    .cases
                    .iter()
                    .flat_map(|case| &case.differences)
                    .filter(|difference| difference.kind == "transport_error")
                    .count(),
                environment_errors: 0,
                environment_blocked: false,
                precondition_errors: 0,
                blocked_cases: self
                    .cases
                    .iter()
                    .filter(|case| case.blocked_by.is_some())
                    .count(),
                incomplete_reason: Some(reason.to_string()),
            },
            cases: std::mem::take(&mut self.cases),
            initial_state_differences: Vec::new(),
            state_differences,
            git_state_differences: Vec::new(),
        };
        let route_coverage_result = update_route_execution(
            &mut self.route_coverage,
            &diff.cases,
            &journal.evidence,
            self.suite,
        )
        .and_then(|_| {
            write_json(
                &self.report_dir.join("route-coverage.json"),
                &self.route_coverage,
            )
        });
        if let Err(error) = route_coverage_result {
            eprintln!("file-server-ab: could not update route coverage: {error:#}");
        }
        if let Err(error) = write_json(&self.report_dir.join("diff.json"), &diff) {
            eprintln!("file-server-ab: could not write partial diff.json: {error:#}");
            return;
        }
        if let Err(error) = write_summary(&self.report_dir.join("summary.md"), run_id, &diff) {
            eprintln!("file-server-ab: could not write partial summary.md: {error:#}");
        }
    }
}

async fn run_suite(options: RunOptions) -> Result<()> {
    let RunOptions {
        suite,
        run_id: requested_run_id,
        rules: rules_path,
        route_coverage: route_coverage_path,
        rust_url,
        ts_url,
        rust_dev_url,
        ts_dev_url,
        rust_root,
        ts_root,
        fixtures,
        report_root,
    } = options;
    let run_id = requested_run_id
        .as_deref()
        .map(validate_run_id)
        .transpose()?
        .unwrap_or_else(|| {
            format!(
                "{}-{}",
                timestamp_for_id(),
                &uuid::Uuid::now_v7().to_string()[..8]
            )
        });
    let report_dir = report_root.join(&run_id);
    fs::create_dir_all(report_dir.join("bodies"))?;
    fs::create_dir_all(report_dir.join("state"))?;
    fs::create_dir_all(report_dir.join("logs"))?;

    prepare_computer_fixture(&rust_root)?;
    prepare_computer_fixture(&ts_root)?;
    if matches!(suite, Suite::Git | Suite::All) {
        prepare_git_fixtures(&rust_root)?;
        prepare_git_fixtures(&ts_root)?;
    }

    let fixture_hashes = hash_fixtures(&fixtures)?;
    let rules_bytes = fs::read(&rules_path)
        .with_context(|| format!("read A/B difference rules {}", rules_path.display()))?;
    let rules: RulesFile = serde_json::from_slice(&rules_bytes)
        .with_context(|| format!("parse A/B difference rules {}", rules_path.display()))?;
    validate_rules(&rules)?;
    let route_coverage_bytes = fs::read(&route_coverage_path).with_context(|| {
        format!(
            "read route coverage inventory {}",
            route_coverage_path.display()
        )
    })?;
    let mut route_coverage: Value =
        serde_json::from_slice(&route_coverage_bytes).with_context(|| {
            format!(
                "parse route coverage inventory {}",
                route_coverage_path.display()
            )
        })?;
    if route_coverage.get("schema_version").and_then(Value::as_u64) != Some(1) {
        bail!("unsupported file-server A/B route coverage schema");
    }
    let route_coverage_baseline = route_coverage
        .get("typescript_revision")
        .and_then(Value::as_str)
        .context("route coverage inventory is missing typescript_revision")?;
    let route_coverage_baseline_matches =
        route_coverage_baseline == env_or("AB_TS_SOURCE", "unknown");
    if !route_coverage_baseline_matches {
        bail!(
            "TypeScript route coverage inventory targets {}, but this run uses {}; update and review tests/file-server-ab/route-coverage.json before comparing",
            route_coverage_baseline,
            env_or("AB_TS_SOURCE", "unknown")
        );
    }
    let entries = route_coverage
        .get_mut("entries")
        .and_then(Value::as_array_mut)
        .context("route coverage inventory is missing entries array")?;
    for entry in entries {
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .context("route coverage entry is missing status")?;
        if !matches!(status, "covered" | "pending" | "intentional-unsupported") {
            bail!("invalid route coverage status {status}");
        }
        let cases = entry
            .get("cases")
            .and_then(Value::as_array)
            .context("route coverage entry is missing cases array")?;
        let planned = cases
            .iter()
            .filter_map(Value::as_str)
            .any(|case| case_matches_suite(case, suite));
        if let Some(object) = entry.as_object_mut() {
            object.insert("planned_in_this_run".into(), json!(planned));
            object.insert("executed_in_this_run".into(), json!(false));
            object.insert("execution_status".into(), json!("not_run"));
        }
    }
    write_json(&report_dir.join("route-coverage.json"), &route_coverage)?;
    let mut endpoints = BTreeMap::new();
    endpoints.insert("rust".to_string(), rust_url.clone());
    endpoints.insert("typescript".to_string(), ts_url.clone());
    for (key, env_name) in [
        ("rust-host", "AB_RUST_HOST_URL"),
        ("typescript-host", "AB_TS_HOST_URL"),
    ] {
        let value = env_or(env_name, "");
        if !value.is_empty() {
            endpoints.insert(key.to_string(), value);
        }
    }
    if !rust_dev_url.is_empty() {
        endpoints.insert("rust-dev".to_string(), rust_dev_url.clone());
    }
    if !ts_dev_url.is_empty() {
        endpoints.insert("typescript-dev".to_string(), ts_dev_url.clone());
    }
    let manifest = Manifest {
        run_id: run_id.clone(),
        suite: suite.as_str().to_string(),
        started_at: timestamp_rfc3339(),
        configuration_profile:
            "isolated-compose-containers-shared-test-core-v8-independent-pnpm-caches".to_string(),
        rust_source: env_or("AB_RUST_SOURCE", "unknown"),
        ts_source: env_or("AB_TS_SOURCE", "unknown"),
        rust_image: env_or("AB_RUST_IMAGE", "unknown"),
        ts_image: env_or("AB_TS_IMAGE", "unknown"),
        docker_builder: env_or("AB_DOCKER_BUILDER", "docker-cli-selected"),
        rust_node_version: env_or("AB_RUST_NODE_VERSION", "unknown"),
        ts_node_version: env_or("AB_TS_NODE_VERSION", "unknown"),
        rust_runtime_architecture: env_or("AB_RUST_NODE_ARCH", "unknown"),
        ts_runtime_architecture: env_or("AB_TS_NODE_ARCH", "unknown"),
        rust_pnpm_version: env_or("AB_RUST_PNPM_VERSION", "unknown"),
        ts_pnpm_version: env_or("AB_TS_PNPM_VERSION", "unknown"),
        pnpm_registry: env_or("AB_PNPM_REGISTRY", "unknown"),
        pnpm_network_concurrency: env_or("AB_PNPM_NETWORK_CONCURRENCY", "unknown"),
        pnpm_rust_store_volume: env_or("AB_PNPM_RUST_STORE_VOLUME", "unknown"),
        pnpm_rust_metadata_cache_volume: env_or("AB_PNPM_RUST_METADATA_CACHE_VOLUME", "unknown"),
        pnpm_typescript_store_volume: env_or("AB_PNPM_TS_STORE_VOLUME", "unknown"),
        pnpm_typescript_metadata_cache_volume: env_or(
            "AB_PNPM_TS_METADATA_CACHE_VOLUME",
            "unknown",
        ),
        rust_git_version: env_or("AB_RUST_GIT_VERSION", "unknown"),
        ts_git_version: env_or("AB_TS_GIT_VERSION", "unknown"),
        fixtures: fixture_hashes,
        rules_sha256: sha256(&rules_bytes),
        route_coverage_sha256: sha256(&route_coverage_bytes),
        route_coverage_baseline_matches,
        endpoints,
    };
    write_json(&report_dir.join("manifest.json"), &manifest)?;

    let client = client()?;
    let mut journal = RequestJournal::new(report_dir.clone())?;
    let rust_health = wait_health(&client, &rust_url, "rust").await;
    let ts_health = wait_health(&client, &ts_url, "typescript").await;
    if rust_health.is_err() || ts_health.is_err() {
        let mut errors = Vec::new();
        if let Err(error) = rust_health {
            errors.push(format!("rust: {error:#}"));
        }
        if let Err(error) = ts_health {
            errors.push(format!("typescript: {error:#}"));
        }
        let diff = RunDiff {
            cases: Vec::new(),
            initial_state_differences: Vec::new(),
            state_differences: Vec::new(),
            git_state_differences: Vec::new(),
            summary: Summary {
                comparisons: 0,
                equal: 0,
                expected_differences: 0,
                unclassified_differences: 0,
                transport_errors: 0,
                environment_errors: 0,
                environment_blocked: true,
                precondition_errors: errors.len(),
                blocked_cases: 0,
                incomplete_reason: None,
            },
        };
        write_json(&report_dir.join("environment-errors.json"), &errors)?;
        write_json(&report_dir.join("diff.json"), &diff)?;
        write_summary(&report_dir.join("summary.md"), &run_id, &diff)?;
        bail!("A/B environment is not ready; see {}", report_dir.display());
    }

    let rust_initial_state = snapshot_roots(&rust_root)?;
    let ts_initial_state = snapshot_roots(&ts_root)?;
    write_json(
        &report_dir.join("state/initial-rust.json"),
        &rust_initial_state,
    )?;
    write_json(
        &report_dir.join("state/initial-typescript.json"),
        &ts_initial_state,
    )?;
    let initial_state_differences = compare_snapshots(&rust_initial_state, &ts_initial_state)?;
    if !initial_state_differences.is_empty() {
        let diff = RunDiff {
            cases: Vec::new(),
            summary: Summary {
                comparisons: 0,
                equal: 0,
                expected_differences: 0,
                unclassified_differences: initial_state_differences.len(),
                transport_errors: 0,
                environment_errors: 0,
                environment_blocked: false,
                precondition_errors: initial_state_differences.len(),
                blocked_cases: 0,
                incomplete_reason: None,
            },
            initial_state_differences,
            state_differences: Vec::new(),
            git_state_differences: Vec::new(),
        };
        write_json(&report_dir.join("diff.json"), &diff)?;
        write_summary(&report_dir.join("summary.md"), &run_id, &diff)?;
        bail!(
            "Rust and TypeScript initial workspace state differs; no A/B scenarios were executed. See {}",
            report_dir.display()
        );
    }

    let mut recorder = Recorder::new(report_dir.clone(), suite, route_coverage)?;
    let suites = async {
        if matches!(suite, Suite::Core | Suite::All) {
            for (case_name, spec) in core_scenarios(&fixtures)? {
                if let Some(blocker) = find_blocker(&recorder.cases, case_dependencies(case_name)) {
                    recorder.record(&journal, blocked_case(case_name, &blocker))?;
                    println!("BLKD {case_name} (blocked by {blocker})");
                    continue;
                }
                let (mut case, rust, ts) = run_pair_specs(
                    &client,
                    &mut journal,
                    case_name,
                    &rust_url,
                    &ts_url,
                    &spec,
                    &spec,
                    RequestTarget::Api,
                )
                .await?;
                for (side, exchange) in [("rust", &rust), ("typescript", &ts)] {
                    let validation = match case_name {
                        "computer-file-meta" => validate_meta_response(&exchange.body),
                        "version" => validate_semver_field(&exchange.body, "version"),
                        "computer-file-meta-boundary" => {
                            validate_boundary_meta_response(&exchange.body, side)
                        }
                        "computer-files-update-mixed-operations" => {
                            validate_files_update_response(&exchange.body, CASE_USER, CASE_CID, 5)
                        }
                        "computer-upload-file-binary" => {
                            validate_upload_file_response(&exchange.body, 7)
                        }
                        "computer-upload-files-mixed-content" => {
                            validate_upload_files_response(&exchange.body)
                        }
                        "computer-read-files-update-content" => {
                            validate_body_equals(&exchange.body, "A/B+%中\n".as_bytes())
                        }
                        "computer-read-uploaded-single-binary" => {
                            validate_body_equals(&exchange.body, &[0, 1, 2, 13, 10, 127, 255])
                        }
                        "computer-read-uploaded-batch-binary" => {
                            validate_body_equals(&exchange.body, &[0, 255, 10, 13, 42])
                        }
                        "computer-create-workspace-v1-local-skills"
                        | "computer-create-workspace-v2-local-config"
                        | "computer-push-skills-v1-local-zip"
                        | "computer-push-skills-v2-local-zip"
                        | "computer-import-project-local-zip"
                        | "computer-init-project-template-package-fixture"
                        | "project-push-skills-local-zip"
                        | "project-upload-single-file-bytes"
                        | "project-upload-batch-mixed-bytes"
                        | "project-upload-project-wrapper-zip" => {
                            validate_success_json(&exchange.body)
                        }
                        "computer-delete-workspace-owned-fixture" => {
                            validate_boolean_field(&exchange.body, "deleted", true)
                        }
                        "computer-generate-file-utf8" => validate_json_string_field(
                            &exchange.body,
                            "fileName",
                            "ab-generated/nested/message.txt",
                        ),
                        "computer-read-generated-file-utf8" => {
                            validate_body_equals(&exchange.body, "A/B + % 中文\n".as_bytes())
                        }
                        "computer-read-imported-project-file"
                        | "project-read-uploaded-project-file" => {
                            validate_body_equals(&exchange.body, b"imported from local fixture\n")
                        }
                        "computer-import-preservation-contract" => validate_execute_command_output(
                            &exchange.body,
                            "import preservation ok\n",
                        ),
                        "computer-init-template-git-tree" => {
                            validate_git_tree_response(&exchange.body)
                        }
                        "computer-install-empty-typescript-project" => validate_json_string_field(
                            &exchange.body,
                            "programmingLanguage",
                            "typescript",
                        ),
                        "computer-build-agent-package-synthetic" => {
                            validate_build_artifact(&exchange.body)
                        }
                        "computer-cleanup-build-artifacts" => {
                            validate_boolean_field(&exchange.body, "cleaned", true)
                        }
                        "computer-execute-command-fixed-output" => {
                            validate_execute_command(&exchange.body)
                        }
                        "computer-get-logs-tail-lines" => validate_log_tail(&exchange.body),
                        "computer-download-all-files-semantic-zip" => {
                            validate_download_archive(&exchange.body, CASE_USER, CASE_CID)
                        }
                        "computer-zip-workspace-semantic" => {
                            validate_workspace_archive(&exchange.body)
                        }
                        "project-all-files-update-replace" => validate_json_string_field(
                            &exchange.body,
                            "projectId",
                            "file-server-ab-project-lifecycle",
                        ),
                        "project-backup-deprecated-under-git"
                        | "project-get-version-deprecated-under-git"
                        | "project-rollback-deprecated-under-git" => {
                            validate_deprecated_json(&exchange.body)
                        }
                        "project-copy-project-tree" => validate_project_copy(&exchange.body),
                        "project-copy-git-history" => {
                            validate_copy_git_history(&exchange.body, side)
                        }
                        "project-upload-attachment-deterministic-name" => {
                            validate_attachment_response(&exchange.body)
                        }
                        "project-delete-owned-fixture" => {
                            validate_project_delete(&exchange.body, "file-server-ab-project-delete")
                        }
                        _ => Ok(()),
                    };
                    if let Err(error) = validation {
                        case.differences.push(assertion_difference(
                            case_name,
                            &format!("/assertions/{side}/contract"),
                            error,
                        ));
                    }
                }
                let upload_temp_dirs = match case_name {
                    "computer-import-project-local-zip" => vec![
                        ("rust", rust_root.join("project-zips").join("temp")),
                        (
                            "typescript",
                            ts_root
                                .join("computer-workspace")
                                .join(CASE_USER)
                                .join(AB_IMPORT_CID)
                                .join(".tmp"),
                        ),
                    ],
                    "project-upload-project-wrapper-zip" => vec![
                        ("rust", rust_root.join("project-zips").join("temp")),
                        ("typescript", ts_root.join("project-zips").join("temp")),
                    ],
                    _ => Vec::new(),
                };
                for (side, path) in upload_temp_dirs {
                    if let Err(error) = validate_directory_empty_or_absent(&path) {
                        case.differences.push(assertion_difference(
                            case_name,
                            &format!("/assertions/{side}/upload-temp-cleanup"),
                            error,
                        ));
                    }
                }
                println!(
                    "{} {}",
                    if case.differences.is_empty() {
                        "PASS"
                    } else {
                        "DIFF"
                    },
                    case.case
                );
                recorder.record(&journal, case)?;
                if case_name == "read-project-static-file" {
                    let case_name = "read-project-static-file-if-none-match";
                    // The conditional request replays the ETag of the previous response; if
                    // that response already failed, a 304 chase would only add cascade noise.
                    if let Some(blocker) =
                        find_blocker(&recorder.cases, &["read-project-static-file"])
                    {
                        recorder.record(&journal, blocked_case(case_name, &blocker))?;
                        println!("BLKD {case_name} (blocked by {blocker})");
                        continue;
                    }
                    let mut rust_spec = get_spec(
                        "/api/page/static/file-server-ab-react/src/file-server-ab.txt".to_string(),
                    );
                    let mut ts_spec = get_spec(
                        "/api/page/static/file-server-ab-react/src/file-server-ab.txt".to_string(),
                    );
                    rust_spec.expected_status = ExpectedStatus::NotModified304;
                    ts_spec.expected_status = ExpectedStatus::NotModified304;
                    // ETags are opaque validators and intentionally differ between Express and
                    // Rust. Each implementation receives its own validator; the contract is that
                    // both return 304 for their unchanged representation.
                    rust_spec.normalized_headers = vec!["etag".into(), "last-modified".into()];
                    ts_spec.normalized_headers = rust_spec.normalized_headers.clone();
                    let rust_etag = rust
                        .headers
                        .get(reqwest::header::ETAG)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let ts_etag = ts
                        .headers
                        .get(reqwest::header::ETAG)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    if let Some(etag) = &rust_etag {
                        rust_spec
                            .headers
                            .insert("if-none-match".into(), etag.clone());
                    }
                    if let Some(etag) = &ts_etag {
                        ts_spec.headers.insert("if-none-match".into(), etag.clone());
                    }
                    let (mut conditional_case, rust_conditional, ts_conditional) = run_pair_specs(
                        &client,
                        &mut journal,
                        case_name,
                        &rust_url,
                        &ts_url,
                        &rust_spec,
                        &ts_spec,
                        RequestTarget::Api,
                    )
                    .await?;
                    for (side, etag, exchange) in [
                        ("rust", rust_etag, &rust_conditional),
                        ("typescript", ts_etag, &ts_conditional),
                    ] {
                        if etag.is_none() {
                            conditional_case.differences.push(assertion_difference(
                                case_name,
                                &format!("/assertions/{side}/etag"),
                                "initial static response did not contain an ETag validator".into(),
                            ));
                        }
                        if !exchange.body.is_empty() {
                            conditional_case.differences.push(assertion_difference(
                                case_name,
                                &format!("/assertions/{side}/304-body"),
                                format!(
                                    "expected an empty 304 response body, got {} bytes",
                                    exchange.body.len()
                                ),
                            ));
                        }
                    }
                    println!(
                        "{} {}",
                        if conditional_case.differences.is_empty() {
                            "PASS"
                        } else {
                            "DIFF"
                        },
                        conditional_case.case
                    );
                    recorder.record(&journal, conditional_case)?;
                }
            }
        }
        if matches!(suite, Suite::Git | Suite::All) {
            let mut rust_commit_hash = None;
            let mut ts_commit_hash = None;
            for scenario in git_scenarios()? {
                let case_name = scenario.name.as_str();
                if let Some(blocker) = find_blocker(&recorder.cases, case_dependencies(case_name)) {
                    recorder.record(&journal, blocked_case(case_name, &blocker))?;
                    println!("BLKD {case_name} (blocked by {blocker})");
                    continue;
                }
                if let Some(preparation) = scenario.preparation {
                    prepare_git_scenario(preparation, &scenario.project_id, &rust_root, &ts_root)?;
                }
                let (mut case, rust, ts) = run_pair_specs(
                    &client,
                    &mut journal,
                    case_name,
                    &rust_url,
                    &ts_url,
                    &scenario.spec,
                    &scenario.spec,
                    RequestTarget::Api,
                )
                .await?;
                match case_name {
                    "git-commit-initial" => {
                        for (side, exchange, hash_slot) in [
                            ("rust", &rust, &mut rust_commit_hash),
                            ("typescript", &ts, &mut ts_commit_hash),
                        ] {
                            match validate_git_commit_response(&exchange.body) {
                                Ok(hash) => *hash_slot = Some(hash),
                                Err(error) => case.differences.push(assertion_difference(
                                    case_name,
                                    &format!("/assertions/{side}/commit"),
                                    error,
                                )),
                            }
                        }
                    }
                    "git-commit-second" => {
                        for (side, exchange, hash_slot) in [
                            ("rust", &rust, &mut rust_commit_hash),
                            ("typescript", &ts, &mut ts_commit_hash),
                        ] {
                            match validate_git_commit_response(&exchange.body) {
                                Ok(hash) => *hash_slot = Some(hash),
                                Err(error) => case.differences.push(assertion_difference(
                                    case_name,
                                    &format!("/assertions/{side}/commit"),
                                    error,
                                )),
                            }
                        }
                    }
                    "git-log" => {
                        for (side, exchange) in [("rust", &rust), ("typescript", &ts)] {
                            let expected_hash = if side == "rust" {
                                rust_commit_hash.as_deref()
                            } else {
                                ts_commit_hash.as_deref()
                            };
                            match validate_git_log_response(&exchange.body) {
                            Ok(log_hash) if Some(log_hash.as_str()) == expected_hash => {}
                            Ok(log_hash) => case.differences.push(assertion_difference(
                                case_name,
                                &format!("/assertions/{side}/log-commit-identity"),
                                format!(
                                    "log hash {log_hash} does not match commit response {expected_hash:?}"
                                ),
                            )),
                            Err(error) => case.differences.push(assertion_difference(
                                case_name,
                                &format!("/assertions/{side}/log"),
                                error,
                            )),
                        }
                        }
                    }
                    "git-status-after-merge-conflict" => {
                        for (side, exchange) in [("rust", &rust), ("typescript", &ts)] {
                            if let Err(error) =
                                validate_git_conflicted_status(&exchange.body, "README.md")
                            {
                                case.differences.push(assertion_difference(
                                    case_name,
                                    &format!("/assertions/{side}/conflicted"),
                                    error,
                                ));
                            }
                        }
                    }
                    _ => {}
                }
                println!(
                    "{} {}",
                    if case.differences.is_empty() {
                        "PASS"
                    } else {
                        "DIFF"
                    },
                    case.case
                );
                recorder.record(&journal, case)?;
            }
        }
        if matches!(suite, Suite::Build | Suite::All) {
            if rust_dev_url.trim().is_empty() || ts_dev_url.trim().is_empty() {
                bail!("build suite requires --rust-dev-url and --ts-dev-url");
            }
            run_build_suite(
                &client,
                &mut journal,
                &mut recorder,
                &rust_url,
                &ts_url,
                &rust_dev_url,
                &ts_dev_url,
            )
            .await?;
        }
        let rust_state = snapshot_roots(&rust_root)?;
        let ts_state = snapshot_roots(&ts_root)?;
        write_json(&report_dir.join("state/rust.json"), &rust_state)?;
        write_json(&report_dir.join("state/typescript.json"), &ts_state)?;
        let mut state_differences = compare_snapshots(&rust_state, &ts_state)?;
        let coverage_inconsistencies = update_route_execution(
            &mut recorder.route_coverage,
            &recorder.cases,
            &journal.evidence,
            suite,
        )?;
        // Routes planned for the selected suite that never produced a complete paired
        // execution (never scheduled, partially executed, blocked, or lacking request
        // evidence) make the run incomplete even when all executed cases are equal.
        let unexecuted_planned: Vec<String> = recorder
            .route_coverage
            .get("entries")
            .and_then(Value::as_array)
            .context("route coverage entries missing")?
            .iter()
            .filter(|entry| {
                entry
                    .get("planned_in_this_run")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    && matches!(
                        entry.get("execution_status").and_then(Value::as_str),
                        Some("not_run") | Some("partial") | Some("blocked") | Some("unverified")
                    )
            })
            .map(|entry| {
                format!(
                    "{} {} ({})",
                    entry.get("method").and_then(Value::as_str).unwrap_or("?"),
                    entry.get("path").and_then(Value::as_str).unwrap_or("?"),
                    entry
                        .get("execution_status")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                )
            })
            .collect();
        if let Some(object) = recorder.route_coverage.as_object_mut() {
            object.insert(
                "coverage_inconsistencies".into(),
                json!(coverage_inconsistencies),
            );
            object.insert(
                "unexecuted_planned_routes".into(),
                json!(unexecuted_planned),
            );
        }
        write_json(
            &report_dir.join("route-coverage.json"),
            &recorder.route_coverage,
        )?;
        let git_suite_ran = matches!(suite, Suite::Git | Suite::All);
        let mut git_state_differences = Vec::new();
        if git_suite_ran {
            let mut rust_git_states = BTreeMap::new();
            let mut ts_git_states = BTreeMap::new();
            for project_id in GIT_PROJECT_IDS {
                let rust_git_state =
                    snapshot_git_state(&rust_root.join("project-workspace").join(project_id))?;
                let ts_git_state =
                    snapshot_git_state(&ts_root.join("project-workspace").join(project_id))?;
                for (side, state) in [("rust", &rust_git_state), ("typescript", &ts_git_state)] {
                    if !state.repository_exists || !state.capture_errors.is_empty() {
                        git_state_differences.push(assertion_difference(
                            "git-state",
                            &format!("/repositories/{project_id}/assertions/{side}/capture"),
                            format!(
                                "could not capture a complete Git state: repository_exists={}, errors={:?}",
                                state.repository_exists, state.capture_errors
                            ),
                        ));
                    }
                }
                rust_git_states.insert(project_id.to_string(), rust_git_state);
                ts_git_states.insert(project_id.to_string(), ts_git_state);
            }
            write_json(&report_dir.join("state/git-rust.json"), &rust_git_states)?;
            write_json(
                &report_dir.join("state/git-typescript.json"),
                &ts_git_states,
            )?;
            let rust_json =
                serde_json::to_value(&rust_git_states).context("serialize Rust Git state")?;
            let ts_json =
                serde_json::to_value(&ts_git_states).context("serialize TypeScript Git state")?;
            json_differences(
                "git-state",
                "",
                &rust_json,
                &ts_json,
                &[],
                &mut git_state_differences,
            );
        }
        for case in &mut recorder.cases {
            apply_rules(&mut case.differences, &rules);
            case.equal = case.differences.is_empty();
        }
        apply_rules(&mut state_differences, &rules);
        apply_rules(&mut git_state_differences, &rules);

        let equal = recorder.cases.iter().filter(|case| case.equal).count()
            + usize::from(initial_state_differences.is_empty())
            + usize::from(state_differences.is_empty())
            + usize::from(git_suite_ran && git_state_differences.is_empty());
        let expected_differences = recorder
            .cases
            .iter()
            .flat_map(|case| &case.differences)
            .filter(|difference| difference.accepted)
            .count()
            + state_differences
                .iter()
                .filter(|difference| difference.accepted)
                .count()
            + initial_state_differences
                .iter()
                .filter(|difference| difference.accepted)
                .count()
            + git_state_differences
                .iter()
                .filter(|difference| difference.accepted)
                .count();
        let unclassified_differences = recorder
            .cases
            .iter()
            .flat_map(|case| &case.differences)
            .filter(|difference| {
                !difference.accepted
                    && !matches!(difference.kind.as_str(), "transport_error" | "blocked")
            })
            .count()
            + state_differences
                .iter()
                .filter(|difference| !difference.accepted)
                .count()
            + initial_state_differences
                .iter()
                .filter(|difference| !difference.accepted)
                .count()
            + git_state_differences
                .iter()
                .filter(|difference| !difference.accepted)
                .count();
        let transport_errors = recorder
            .cases
            .iter()
            .flat_map(|case| &case.differences)
            .filter(|difference| difference.kind == "transport_error")
            .count();
        let blocked_cases = recorder
            .cases
            .iter()
            .filter(|case| case.blocked_by.is_some())
            .count();
        let diff = RunDiff {
            summary: Summary {
                comparisons: recorder.cases.len() + 2 + usize::from(git_suite_ran),
                equal,
                expected_differences,
                unclassified_differences,
                transport_errors,
                environment_errors: 0,
                environment_blocked: false,
                precondition_errors: 0,
                blocked_cases,
                incomplete_reason: None,
            },
            cases: recorder.cases.clone(),
            initial_state_differences,
            state_differences,
            git_state_differences,
        };
        write_json(&report_dir.join("diff.json"), &diff)?;
        write_summary(&report_dir.join("summary.md"), &run_id, &diff)?;
        println!("report: {}", report_dir.display());
        println!(
            "summary: {}/{} equal; {} expected; {} unclassified; {} blocked",
            diff.summary.equal,
            diff.summary.comparisons,
            diff.summary.expected_differences,
            diff.summary.unclassified_differences,
            diff.summary.blocked_cases
        );
        let final_summary = diff.summary.clone();
        Ok::<(Summary, Vec<String>, Vec<String>), anyhow::Error>((
            final_summary,
            coverage_inconsistencies,
            unexecuted_planned,
        ))
    };
    let suites_outcome = suites.await;
    let (final_summary, coverage_inconsistencies, unexecuted_planned) = match suites_outcome {
        Ok(values) => values,
        Err(error) => {
            recorder.write_incomplete(
                &journal,
                &run_id,
                &rules,
                &rust_root,
                &ts_root,
                &format!("{error:#}"),
            );
            bail!(
                "A/B run exited before a complete report: {error:#}; partial evidence was preserved in {}",
                report_dir.display()
            );
        }
    };
    if final_summary.unclassified_differences > 0 {
        bail!("A/B run contains unclassified differences; inspect diff.json and summary.md");
    }
    if final_summary.transport_errors > 0 {
        bail!(
            "A/B run has HTTP transport failures (cause unclassified); inspect requests.jsonl and summary.md"
        );
    }
    if !coverage_inconsistencies.is_empty() {
        bail!(
            "route coverage cross-validation failed ({}); claimed cases had no matching API request on both sides, see route-coverage.json",
            coverage_inconsistencies.len()
        );
    }
    if !unexecuted_planned.is_empty() {
        bail!(
            "A/B run is incomplete: {} route(s) planned for this suite were not executed on both sides: {}",
            unexecuted_planned.len(),
            unexecuted_planned.join(", ")
        );
    }
    Ok(())
}

fn core_scenarios(fixtures: &Path) -> Result<Vec<(&'static str, RequestSpec)>> {
    let json_request = |method: Method, path: String, body: Value| -> Result<RequestSpec> {
        Ok(RequestSpec {
            method,
            path,
            body: serde_json::to_vec(&body).context("serialize A/B request body")?,
            content_type: Some("application/json"),
            headers: BTreeMap::new(),
            health_probe: false,
            expected_status: ExpectedStatus::Success2xx,
            normalized_paths: Vec::new(),
            normalized_headers: Vec::new(),
            timeout: Duration::from_secs(30),
        })
    };
    let get = |path: String| RequestSpec {
        method: Method::GET,
        path,
        body: Vec::new(),
        content_type: None,
        headers: BTreeMap::new(),
        health_probe: false,
        expected_status: ExpectedStatus::Success2xx,
        normalized_paths: Vec::new(),
        normalized_headers: Vec::new(),
        timeout: Duration::from_secs(30),
    };
    let get_client_error = |path: String| RequestSpec {
        expected_status: ExpectedStatus::ClientError4xx,
        normalized_paths: vec!["/error/requestId".into(), "/error/timestamp".into()],
        normalized_headers: Vec::new(),
        ..get(path)
    };
    let user = CASE_USER;
    let cid = CASE_CID;
    let root = "/data/computer-workspace/file-server-ab-user/file-server-ab-session";
    let skills_zip =
        fs::read(fixtures.join("skills-fixture.zip")).context("read local skills A/B fixture")?;
    let workspace_zip = fs::read(fixtures.join("workspace-project.zip"))
        .context("read local workspace ZIP A/B fixture")?;
    let package_zip = fs::read(fixtures.join("package-project.zip"))
        .context("read local package-project A/B fixture")?;
    let mut file_meta = json_request(
        Method::POST,
        "/api/computer/get-file-meta".to_string(),
        json!({
            "userId": user,
            "cId": cid,
            "filePaths": ["README.md", "sub/nested/hello.txt"]
        }),
    )?;
    // Separate fixture trees are created milliseconds apart. Compare every stable metadata
    // field and validate that both timestamps are present; preserve their raw values in bodies.
    file_meta.normalized_paths = vec!["/metas/0/mtimeMs".into(), "/metas/1/mtimeMs".into()];
    let boundary_meta = json_request(
        Method::POST,
        "/api/computer/get-file-meta".to_string(),
        json!({
            "userId": user,
            "cId": cid,
            "filePaths": [".hidden.txt", "  中文文件 .txt  ", "binary.bin"]
        }),
    )?;
    let mut boundary_meta = boundary_meta;
    // These files are created independently on each side. Preserve their raw mtimes in
    // requests.jsonl, but don't mistake millisecond creation-time skew for API drift.
    // metas/1 is the leading/trailing-space file name: Rust addresses the exact path
    // and returns full metadata; TypeScript trims the name and reports ENOENT. That
    // divergence is approved (exact file names win), so the whole entry is compared
    // semantically with per-side assertions below instead of field-by-field.
    boundary_meta.normalized_paths = vec![
        "/metas/0/mtimeMs".into(),
        "/metas/2/mtimeMs".into(),
        "/metas/1".into(),
    ];
    let single_upload_bytes: &[u8] = &[0, 1, 2, 13, 10, 127, 255];
    let single_upload_body = multipart_form_body(
        AB_MULTIPART_BOUNDARY,
        &[
            ("userId", user),
            ("cId", cid),
            ("filePath", "ab-transfer/single/nested/payload.bin"),
        ],
        &[("file", "payload.bin", single_upload_bytes)],
    );
    let batch_first_bytes: &[u8] = b"batch upload one\n";
    let batch_second_bytes: &[u8] = &[0, 255, 10, 13, 42];
    let batch_upload_body = multipart_form_body(
        AB_MULTIPART_BOUNDARY,
        &[
            ("userId", user),
            ("cId", cid),
            ("filePaths", "ab-transfer/batch/one.txt"),
            ("filePaths", "ab-transfer/batch/nested/two.bin"),
        ],
        &[
            ("files", "one.txt", batch_first_bytes),
            ("files", "two.bin", batch_second_bytes),
        ],
    );
    let static_read =
        get("/api/page/static/file-server-ab-react/src/file-server-ab.txt".to_string());
    // etag/last-modified 由 header 协议语义层校验 (静态内容要求两侧存在且格式合法,
    // 值可不同), 不再整头跳过比较。
    let mut static_range = get("/api/page/static/file-server-ab-react/package.json".to_string());
    static_range
        .headers
        .insert("range".into(), "bytes=0-5".into());
    static_range.expected_status = ExpectedStatus::PartialContent206;
    let mut scenarios = vec![
        (
            "health",
            RequestSpec {
                method: Method::GET,
                path: "/health".to_string(),
                body: Vec::new(),
                content_type: None,
                headers: BTreeMap::new(),
                health_probe: true,
                expected_status: ExpectedStatus::Success2xx,
                normalized_paths: Vec::new(),
                normalized_headers: Vec::new(),
                timeout: Duration::from_secs(30),
            },
        ),
        ("root", get("/".to_string())),
        // 版本是各端契约线声明 (Java 据此做能力门禁)，值不要求跨实现相等；
        // 由断言校验语义版本形状。
        (
            "version",
            RequestSpec {
                normalized_paths: vec!["/version".into()],
                ..get("/api/version".to_string())
            },
        ),
        (
            "create-react-template-project",
            project_create_spec("file-server-ab-react", "react")?,
        ),
        (
            "read-react-template-project",
            get("/api/project/get-project-content?projectId=file-server-ab-react&proxyPath=%2Fproxy".to_string()),
        ),
        (
            "update-project-file",
            json_request(
                Method::POST,
                "/api/project/specified-files-update".to_string(),
                json!({
                    "projectId":"file-server-ab-react",
                    "codeVersion":"1",
                    "files":[{"operation":"create","name":"src/file-server-ab.txt","contents":"A%2FB+comparison%0A"}]
                }),
            )?,
        ),
        (
            "read-project-static-file",
            static_read,
        ),
        ("read-project-static-file-range", static_range),
        (
            "create-vue-template-project",
            project_create_spec("file-server-ab-vue", "vue3")?,
        ),
        (
            "read-vue-template-project",
            get("/api/project/get-project-content?projectId=file-server-ab-vue&proxyPath=%2Fproxy".to_string()),
        ),
        (
            "computer-file-list-all",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&proxyPath=%2Fproxy")),
        ),
        (
            "computer-file-list-boundary-fixtures",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&type=file&proxyPath=%2Fproxy")),
        ),
        (
            "computer-file-list-files-limited",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&type=file&limit=2")),
        ),
        (
            "computer-file-list-empty-directories",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&type=dir")),
        ),
        (
            "computer-file-list-subdirectory",
            get(format!("/api/computer/get-file-list?userId={user}&cId={cid}&relativePath=sub")),
        ),
        (
            "computer-resolve-file-existing",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath=sub%2Fnested%2Fhello.txt&proxyPath=%2Fproxy"
            )),
        ),
        (
            "computer-resolve-file-hidden",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath={}&proxyPath=%2Fproxy",
                encode_query(".hidden.txt")
            )),
        ),
        (
            "computer-resolve-file-symlink-inside",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath={}&proxyPath=%2Fproxy",
                encode_query("inside-link.txt")
            )),
        ),
        (
            "computer-resolve-file-symlink-outside-root",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath={}&proxyPath=%2Fproxy",
                encode_query("outside-link.txt")
            )),
        ),
        (
            "computer-resolve-file-path-traversal",
            get(format!(
                "/api/computer/resolve-file?userId={user}&cId={cid}&filePath={}&proxyPath=%2Fproxy",
                encode_query("../../file-server-ab-outside-secret.txt")
            )),
        ),
        (
            "computer-search-files",
            get(format!(
                "/api/computer/search-files?userId={user}&cId={cid}&kw=hello&limit=10&maxVisit=100&timeoutMs=1000"
            )),
        ),
        ("computer-file-meta", file_meta),
        ("computer-file-meta-boundary", boundary_meta),
        (
            "computer-files-update-mixed-operations",
            json_request(
                Method::POST,
                "/api/computer/files-update".to_string(),
                json!({
                    "userId": user,
                    "cId": cid,
                    "files": [
                        {"operation":"create", "name":"ab-write", "isDir":true},
                        {"operation":"create", "name":"ab-write/created.txt", "contents":"A%2FB%2B%25%E4%B8%AD%0A"},
                        {"operation":"modify", "name":"README.md", "contents":"Updated%20README%0A"},
                        {"operation":"rename", "name":"ab-write/renamed.txt", "renameFrom":"ab-write/created.txt"},
                        {"operation":"delete", "name":"sub/nested/hello.txt"}
                    ]
                }),
            )?,
        ),
        (
            "computer-read-files-update-content",
            get(format!(
                "/api/computer/static/{user}/{cid}/ab-write/renamed.txt"
            )),
        ),
        (
            "computer-upload-file-binary",
            RequestSpec {
                method: Method::POST,
                path: "/api/computer/upload-file".to_string(),
                body: single_upload_body,
                content_type: Some("multipart/form-data; boundary=----file-server-ab-boundary-6d86f5"),
                headers: BTreeMap::new(),
                health_probe: false,
                expected_status: ExpectedStatus::Success2xx,
                normalized_paths: Vec::new(),
                normalized_headers: Vec::new(),
                timeout: Duration::from_secs(30),
            },
        ),
        (
            "computer-read-uploaded-single-binary",
            get(format!(
                "/api/computer/static/{user}/{cid}/ab-transfer/single/nested/payload.bin"
            )),
        ),
        (
            "computer-upload-files-mixed-content",
            RequestSpec {
                method: Method::POST,
                path: "/api/computer/upload-files".to_string(),
                body: batch_upload_body,
                content_type: Some("multipart/form-data; boundary=----file-server-ab-boundary-6d86f5"),
                headers: BTreeMap::new(),
                health_probe: false,
                expected_status: ExpectedStatus::Success2xx,
                normalized_paths: Vec::new(),
                normalized_headers: Vec::new(),
                timeout: Duration::from_secs(30),
            },
        ),
        (
            "computer-read-uploaded-batch-binary",
            get(format!(
                "/api/computer/static/{user}/{cid}/ab-transfer/batch/nested/two.bin"
            )),
        ),
        (
            "computer-file-list-invalid-type",
            get_client_error(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&type=invalid"
            )),
        ),
        (
            "computer-file-list-invalid-limit",
            get_client_error(format!(
                "/api/computer/get-file-list?userId={user}&cId={cid}&limit=-1"
            )),
        ),
        (
            "fs-roots",
            get("/api/computer/fs/roots".to_string()),
        ),
        (
            "fs-children",
            get(format!("/api/computer/fs/children?path={}", encode_query(root))),
        ),
        (
            "fs-mkdir-preserve-spaces",
            json_request(
                Method::POST,
                "/api/computer/fs/mkdir".to_string(),
                json!({"parentPath":root,"dirName":"  A-B spaced dir  "}),
            )?,
        ),
        (
            "fs-rename-preserve-spaces",
            json_request(
                Method::POST,
                "/api/computer/fs/rename".to_string(),
                json!({"path":format!("{root}/  A-B spaced dir  "),"newName":" renamed dir "}),
            )?,
        ),
    ];

    scenarios.extend(core_lifecycle_scenarios(
        &skills_zip,
        &workspace_zip,
        &package_zip,
    )?);
    Ok(scenarios)
}

fn multipart_form_body(
    boundary: &str,
    text_fields: &[(&str, &str)],
    file_fields: &[(&str, &str, &[u8])],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in text_fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    for (field_name, file_name, bytes) in file_fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{file_name}\"\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

fn multipart_spec(
    path: impl Into<String>,
    text_fields: &[(&str, &str)],
    file_fields: &[(&str, &str, &[u8])],
) -> RequestSpec {
    RequestSpec {
        method: Method::POST,
        path: path.into(),
        body: multipart_form_body(AB_MULTIPART_BOUNDARY, text_fields, file_fields),
        content_type: Some("multipart/form-data; boundary=----file-server-ab-boundary-6d86f5"),
        headers: BTreeMap::new(),
        health_probe: false,
        expected_status: ExpectedStatus::Success2xx,
        normalized_paths: Vec::new(),
        normalized_headers: Vec::new(),
        timeout: Duration::from_secs(45),
    }
}

fn core_lifecycle_scenarios(
    skills_zip: &[u8],
    workspace_zip: &[u8],
    package_zip: &[u8],
) -> Result<Vec<(&'static str, RequestSpec)>> {
    let mut scenarios = Vec::new();
    let user = CASE_USER;
    let project = "file-server-ab-project-lifecycle";
    let upload_project = "file-server-ab-project-upload";
    let delete_project = "file-server-ab-project-delete";

    // Project fixtures are created through the public API, then mutated and consumed in order.
    for (case, project_id, template_type) in [
        ("project-fixture-lifecycle-create", project, "react"),
        ("project-fixture-upload-create", upload_project, "react"),
        ("project-fixture-delete-create", delete_project, "vue3"),
    ] {
        scenarios.push((case, project_create_spec(project_id, template_type)?));
    }

    let skills_fields = [("userId", user), ("cId", AB_SKILLS_V1_CID)];
    scenarios.push((
        "computer-create-workspace-v1-local-skills",
        multipart_spec(
            "/api/computer/create-workspace",
            &skills_fields,
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));
    scenarios.push((
        "computer-push-skills-v1-local-zip",
        multipart_spec(
            "/api/computer/push-skills-to-workspace",
            &skills_fields,
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));

    let v2_fields = [
        ("userId", user),
        ("cId", AB_SKILLS_V2_CID),
        ("agentId", "ab-agent"),
        ("skillUrls", "[]"),
        ("skillNames", "[\"file-server-ab-skill\"]"),
        ("mcpServersConfig", "{\"servers\":[]}"),
        ("hooksConfig", "{\"hooks\":[]}"),
        ("permissionsConfig", "{\"allow\":[]}"),
        ("hookScripts", "[]"),
    ];
    scenarios.push((
        "computer-create-workspace-v2-local-config",
        multipart_spec(
            "/api/computer/create-workspace-v2",
            &v2_fields,
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));
    let v2_push_fields = [
        ("userId", user),
        ("cId", AB_SKILLS_V2_CID),
        ("agentId", "ab-agent"),
        ("skillUrls", "[]"),
    ];
    scenarios.push((
        "computer-push-skills-v2-local-zip",
        multipart_spec(
            "/api/computer/push-skills-to-workspace-v2",
            &v2_push_fields,
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));

    scenarios.push((
        "computer-generate-file-utf8",
        json_spec(
            Method::POST,
            "/api/computer/generate-file",
            json!({
                "userId": user,
                "cId": CASE_CID,
                "fileName": "ab-generated/nested/message.txt",
                "content": "A/B + % 中文\n"
            }),
        )?,
    ));
    scenarios.push((
        "computer-read-generated-file-utf8",
        get_spec(format!(
            "/api/computer/static/{user}/{CASE_CID}/ab-generated/nested/message.txt"
        )),
    ));

    let import_fields = [("userId", user), ("cId", AB_IMPORT_CID)];
    scenarios.push((
        "computer-import-project-local-zip",
        multipart_spec(
            "/api/computer/import-project",
            &import_fields,
            &[("file", "workspace-project.zip", workspace_zip)],
        ),
    ));
    scenarios.push((
        "computer-read-imported-project-file",
        get_spec(format!(
            "/api/computer/static/{user}/{AB_IMPORT_CID}/src/imported.txt"
        )),
    ));
    scenarios.push((
        "computer-import-preservation-contract",
        json_spec(
            Method::POST,
            "/api/computer/execute-command",
            json!({
                "userId": user,
                "cId": AB_IMPORT_CID,
                "command": "test \"$(cat .agents/keep.txt)\" = \"preserved agent data\" && test ! -e .agents/from-archive.txt && printf 'import preservation ok\\n'"
            }),
        )?,
    ));

    let init_fields = [
        ("userId", user),
        ("cId", AB_PACKAGE_CID),
        ("enableGit", "true"),
    ];
    scenarios.push((
        "computer-init-project-template-package-fixture",
        multipart_spec(
            "/api/computer/init-project-template",
            &init_fields,
            &[("file", "package-project.zip", package_zip)],
        ),
    ));
    scenarios.push((
        "computer-init-template-git-tree",
        json_spec(
            Method::POST,
            "/api/computer/execute-command",
            json!({
                "userId": user,
                "cId": AB_PACKAGE_CID,
                "command": "git show -s --format=%T HEAD"
            }),
        )?,
    ));
    scenarios.push((
        "computer-install-empty-typescript-project",
        json_spec(
            Method::POST,
            "/api/computer/install-project",
            json!({
                "userId": user,
                "cId": AB_PACKAGE_CID,
                "programmingLanguage": "typescript"
            }),
        )?,
    ));
    scenarios.push((
        "computer-build-agent-package-synthetic",
        json_spec(
            Method::POST,
            "/api/computer/build-agent-package",
            json!({
                "userId": user,
                "cId": AB_PACKAGE_CID,
                "agentId": "17",
                "version": "1.2.3"
            }),
        )?,
    ));
    scenarios.push((
        "computer-cleanup-build-artifacts",
        json_spec(
            Method::POST,
            "/api/computer/cleanup-build-artifacts",
            json!({ "userId": user, "cId": AB_PACKAGE_CID }),
        )?,
    ));

    scenarios.push((
        "computer-execute-command-fixed-output",
        json_spec(
            Method::POST,
            "/api/computer/execute-command",
            json!({
                "userId": user,
                "cId": CASE_CID,
                "command": "printf 'file-server-ab-command-ok\\n'"
            }),
        )?,
    ));
    scenarios.push((
        "computer-get-logs-tail-lines",
        get_spec(format!(
            "/api/computer/get-logs?userId={user}&cId={AB_LOG_CID}&tailLines=2"
        )),
    ));
    scenarios.push((
        "computer-download-all-files-semantic-zip",
        get_spec(format!(
            "/api/computer/download-all-files?userId={user}&cId={CASE_CID}"
        )),
    ));
    scenarios.push((
        "computer-zip-workspace-semantic",
        json_spec(
            Method::POST,
            "/api/computer/zip-workspace",
            json!({ "userId": user, "cId": CASE_CID, "excludeDirs": ["empty"] }),
        )?,
    ));
    scenarios.push((
        "computer-delete-workspace-owned-fixture",
        json_spec(
            Method::POST,
            "/api/computer/delete-workspace",
            json!({ "userId": user, "cId": AB_SKILLS_V1_CID }),
        )?,
    ));

    scenarios.push((
        "project-all-files-update-seed-obsolete",
        json_spec(
            Method::POST,
            "/api/project/specified-files-update",
            json!({
                "projectId": project,
                "codeVersion": "1",
                "files": [{"operation":"create","name":"ab-obsolete.txt","contents":"remove%20me"}]
            }),
        )?,
    ));
    scenarios.push((
        "project-all-files-update-replace",
        json_spec(
            Method::POST,
            "/api/project/all-files-update",
            json!({
                "projectId": project,
                "codeVersion": "2",
                "files": [
                    { "name": "README.md", "contents": "full%20snapshot%0A", "binary": false },
                    { "name": "src/index.html", "contents": "<main>A%2FB%20project</main>%0A", "binary": false },
                    { "name": "empty.txt", "contents": "", "binary": false },
                    { "name": "pnpm-lock.yaml", "contents": "lockfile%20at%20project%20root%0A", "binary": false },
                    { "name": "packages/frontend/package-lock.json", "contents": "%7B%22lockfileVersion%22%3A3%7D%0A", "binary": false }
                ]
            }),
        )?,
    ));
    let mut removed_file_check = get_spec(format!("/api/page/static/{project}/ab-obsolete.txt"));
    removed_file_check.expected_status = ExpectedStatus::ClientError4xx;
    removed_file_check.normalized_paths =
        vec!["/error/requestId".into(), "/error/timestamp".into()];
    scenarios.push((
        "project-all-files-update-removes-omitted-file",
        removed_file_check,
    ));
    scenarios.push((
        "project-upload-single-file-bytes",
        multipart_spec(
            "/api/project/upload-single-file",
            &[
                ("projectId", project),
                ("codeVersion", "3"),
                ("filePath", "src/single.bin"),
            ],
            &[("file", "single.bin", &[0, 1, 2, 13, 10, 127, 255])],
        ),
    ));
    scenarios.push((
        "project-upload-batch-mixed-bytes",
        multipart_spec(
            "/api/project/upload-batch-files",
            &[
                ("projectId", project),
                ("codeVersion", "4"),
                ("filePaths", "src/batch/one.txt"),
                ("filePaths", "src/batch/two.bin"),
            ],
            &[
                ("files", "one.txt", b"batch project file\n"),
                ("files", "two.bin", &[0, 255, 10, 13, 42]),
            ],
        ),
    ));
    scenarios.push((
        "project-upload-attachment-deterministic-name",
        multipart_spec(
            "/api/project/upload-attachment-file",
            &[("projectId", project), ("fileName", "ab-attachment.txt")],
            &[("file", "ab-attachment.txt", b"attachment fixture\n")],
        ),
    ));
    scenarios.push((
        "project-push-skills-local-zip",
        multipart_spec(
            "/api/project/push-skills-to-workspace",
            &[("projectId", project)],
            &[("file", "skills-fixture.zip", skills_zip)],
        ),
    ));
    scenarios.push((
        "project-backup-deprecated-under-git",
        json_spec(
            Method::POST,
            "/api/project/backup-current-version",
            json!({ "projectId": project, "codeVersion": "4" }),
        )?,
    ));
    scenarios.push((
        "project-get-version-deprecated-under-git",
        get_spec(format!(
            "/api/project/get-project-content-by-version?projectId={project}&codeVersion=4"
        )),
    ));
    scenarios.push((
        "project-rollback-deprecated-under-git",
        json_spec(
            Method::POST,
            "/api/project/rollback-version",
            json!({ "projectId": project, "codeVersion": "4", "rollbackTo": "1" }),
        )?,
    ));
    scenarios.push((
        "project-copy-project-tree",
        json_spec(
            Method::POST,
            "/api/project/copy-project",
            json!({ "sourceProjectId": project, "targetProjectId": "file-server-ab-project-copy" }),
        )?,
    ));
    // 已批准分歧 (2026-09-26 确认): 项目复制不复制源 Git 历史。Rust 副本仅含
    // copy commit; TS 复制 .git 保留源历史再追加 copy commit。提交对象本身含
    // 每次运行不同的 hash/date，无法逐值登记规则——整个列表按形状归一，两侧的
    // 已批准形状由 validate_copy_git_history 分侧断言。
    let mut copied_project_git_log = get_spec(
        "/api/git/log?workspaceType=pageApp&projectId=file-server-ab-project-copy&maxCount=20",
    );
    copied_project_git_log.normalized_paths = vec!["/commits".into(), "/total".into()];
    scenarios.push(("project-copy-git-history", copied_project_git_log));
    let mut export_spec = json_spec(
        Method::POST,
        "/api/project/export-project",
        json!({ "projectId": project, "codeVersion": "4", "exportType": "LATEST" }),
    )?;
    // 已批准 (2026-09-26 确认): POST 下载响应不补发 Express 框架头。浏览器不缓存
    // POST 响应、不对 POST 发 Range 请求、Last-Modified 只是当次生成时间——TS 由
    // sendFile 路径带出的这三个头无消费者，Rust 不模仿；ETag 已由 header 协议
    // 语义层按非静态路由处理。
    export_spec.normalized_headers = vec![
        "accept-ranges".into(),
        "cache-control".into(),
        "last-modified".into(),
    ];
    scenarios.push(("project-export-latest-semantic-zip", export_spec));
    scenarios.push((
        "project-upload-project-wrapper-zip",
        multipart_spec(
            "/api/project/upload-project",
            &[("projectId", upload_project), ("codeVersion", "5")],
            &[("file", "workspace-project.zip", workspace_zip)],
        ),
    ));
    scenarios.push((
        "project-read-uploaded-project-file",
        get_spec(format!(
            "/api/page/static/{upload_project}/src/imported.txt"
        )),
    ));
    scenarios.push((
        "project-delete-owned-fixture",
        get_spec(format!(
            "/api/project/delete-project?projectId={delete_project}"
        )),
    ));

    Ok(scenarios)
}

fn validate_files_update_response(
    body: &[u8],
    expected_user: &str,
    expected_cid: &str,
    expected_count: u64,
) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("files-update response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true)
        || value.get("userId").and_then(Value::as_str) != Some(expected_user)
        || value.get("cId").and_then(Value::as_str) != Some(expected_cid)
        || value.get("filesCount").and_then(Value::as_u64) != Some(expected_count)
    {
        return Err(format!(
            "files-update response must confirm success, workspace identity, and {expected_count} operations; got {value}"
        ));
    }
    Ok(())
}

fn validate_upload_file_response(body: &[u8], expected_size: u64) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("upload-file response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true)
        || value.get("fileSize").and_then(Value::as_u64) != Some(expected_size)
    {
        return Err(format!(
            "upload-file response must confirm success and {expected_size} bytes; got {value}"
        ));
    }
    Ok(())
}

fn validate_upload_files_response(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("upload-files response is not JSON: {error}"))?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .filter(|results| results.len() == 2)
        .ok_or_else(|| "upload-files response must contain two results".to_string())?;
    let expected = [
        ("ab-transfer/batch/one.txt", "one.txt", 17),
        ("ab-transfer/batch/nested/two.bin", "two.bin", 5),
    ];
    if value.get("success").and_then(Value::as_bool) != Some(true)
        || value.get("totalCount").and_then(Value::as_u64) != Some(2)
        || value.get("successCount").and_then(Value::as_u64) != Some(2)
        || value.get("failCount").and_then(Value::as_u64) != Some(0)
    {
        return Err(format!(
            "upload-files response must report two successful files and zero failures; got {value}"
        ));
    }
    for (result, (path, name, size)) in results.iter().zip(expected) {
        if result.get("success").and_then(Value::as_bool) != Some(true)
            || result.get("filePath").and_then(Value::as_str) != Some(path)
            || result.get("originalname").and_then(Value::as_str) != Some(name)
            || result.get("fileSize").and_then(Value::as_u64) != Some(size)
        {
            return Err(format!(
                "upload-files result for {path} must preserve path, filename, and {size}-byte size; got {result}"
            ));
        }
    }
    Ok(())
}

fn validate_body_equals(body: &[u8], expected: &[u8]) -> Result<(), String> {
    if body == expected {
        Ok(())
    } else {
        Err(format!(
            "file body differs: expected {} bytes sha256={}, got {} bytes sha256={}",
            expected.len(),
            sha256(expected),
            body.len(),
            sha256(body)
        ))
    }
}

fn validate_directory_empty_or_absent(path: &Path) -> Result<(), String> {
    let mut entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read temporary upload directory: {error}")),
    };
    match entries.next() {
        None => Ok(()),
        Some(Ok(entry)) => Err(format!(
            "temporary upload directory still contains {}",
            entry.file_name().to_string_lossy()
        )),
        Some(Err(error)) => Err(format!("read temporary upload directory entry: {error}")),
    }
}

fn validate_success_json(body: &[u8]) -> Result<(), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!("response must contain success=true; got {value}"))
    }
}

fn validate_boolean_field(body: &[u8], field: &str, expected: bool) -> Result<(), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("response is not JSON: {error}"))?;
    if value.get(field).and_then(Value::as_bool) == Some(expected) {
        Ok(())
    } else {
        Err(format!(
            "response field {field} must be {expected}; got {value}"
        ))
    }
}

fn validate_json_string_field(body: &[u8], field: &str, expected: &str) -> Result<(), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get(field).and_then(Value::as_str) == Some(expected)
    {
        Ok(())
    } else {
        Err(format!(
            "response must contain success=true and {field}={expected:?}; got {value}"
        ))
    }
}

fn validate_deprecated_json(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("deprecated response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(false)
        && value.get("deprecated").and_then(Value::as_bool) == Some(true)
    {
        Ok(())
    } else {
        Err(format!(
            "response must explicitly mark the route deprecated; got {value}"
        ))
    }
}

fn validate_execute_command(body: &[u8]) -> Result<(), String> {
    validate_execute_command_output(body, "file-server-ab-command-ok\n")
}

fn validate_execute_command_output(body: &[u8], expected_stdout: &str) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("execute-command response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("exitCode").and_then(Value::as_i64) == Some(0)
        && value.get("stdout").and_then(Value::as_str) == Some(expected_stdout)
        && value.get("stderr").and_then(Value::as_str) == Some("")
    {
        Ok(())
    } else {
        Err(format!(
            "execute-command output did not match the expected command result; got {value}"
        ))
    }
}

fn validate_git_tree_response(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("git tree response is not JSON: {error}"))?;
    let tree = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let is_hex_tree =
        matches!(tree.len(), 40 | 64) && tree.bytes().all(|byte| byte.is_ascii_hexdigit());
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("exitCode").and_then(Value::as_i64) == Some(0)
        && value.get("stderr").and_then(Value::as_str) == Some("")
        && is_hex_tree
    {
        Ok(())
    } else {
        Err(format!(
            "init-project-template must create a Git commit with a tree hash; got {value}"
        ))
    }
}

fn validate_log_tail(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("get-logs response is not JSON: {error}"))?;
    let logs = value.get("logs").and_then(Value::as_array);
    let expected = [
        json!({"line": 3, "content": "third line"}),
        json!({"line": 4, "content": "fourth line"}),
    ];
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("totalLines").and_then(Value::as_u64) == Some(4)
        && value.get("startIndex").and_then(Value::as_u64) == Some(3)
        && value.get("logFileName").and_then(Value::as_str) == Some("ab.log")
        && logs.is_some_and(|actual| actual == &expected)
    {
        Ok(())
    } else {
        Err(format!(
            "get-logs did not return the expected two-line tail; got {value}"
        ))
    }
}

fn validate_build_artifact(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("build-agent-package response is not JSON: {error}"))?;
    let artifacts = value.get("artifacts").and_then(Value::as_array);
    let matches = artifacts.is_some_and(|items| {
        items.len() == 1
            && items[0].get("fileName").and_then(Value::as_str)
                == Some("agent-17-linux-x64-1.2.3.zip")
            && items[0].get("platform").and_then(Value::as_str) == Some("linux-x64")
            && items[0]
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("/agent-17-linux-x64-1.2.3.zip"))
    });
    if value.get("success").and_then(Value::as_bool) == Some(true) && matches {
        Ok(())
    } else {
        Err(format!(
            "build-agent-package artifact discovery differs from fixture; got {value}"
        ))
    }
}

/// Approved copy semantics (confirmed 2026-09-26): copies do NOT inherit the source
/// Git history. Rust's copy repo contains exactly the copy commit; TypeScript's
/// copy carries the source's init commit plus the copy commit (its known behavior
/// of copying `.git`, remotes included).
fn validate_copy_git_history(body: &[u8], side: &str) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("copy git log response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("copy git log must contain success=true".into());
    }
    let copy_message =
        "copy project: file-server-ab-project-lifecycle -> file-server-ab-project-copy";
    let commits = value
        .get("commits")
        .and_then(Value::as_array)
        .ok_or_else(|| "copy git log must contain a commits array".to_string())?;
    let message_of = |index: usize| {
        commits
            .get(index)
            .and_then(|commit| commit.get("message"))
            .and_then(Value::as_str)
            .map(str::trim_end)
            .unwrap_or_default()
            .to_string()
    };
    let total = value.get("total").and_then(Value::as_u64);
    if side == "rust" {
        if total != Some(1) || commits.len() != 1 || message_of(0) != copy_message {
            return Err(format!(
                "Rust copy history must contain only the copy commit; total={total:?}, commits={commits_len}",
                commits_len = commits.len()
            ));
        }
    } else if total != Some(2)
        || commits.len() != 2
        || message_of(0) != copy_message
        || !message_of(1).starts_with("init project: file-server-ab-project-lifecycle")
    {
        return Err(format!(
            "TypeScript copy history must contain the source init commit plus the copy commit; total={total:?}, commits={commits_len}",
            commits_len = commits.len()
        ));
    }
    Ok(())
}

fn validate_project_copy(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("copy-project response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("sourceProjectId").and_then(Value::as_str)
            == Some("file-server-ab-project-lifecycle")
        && value.get("targetProjectId").and_then(Value::as_str)
            == Some("file-server-ab-project-copy")
    {
        Ok(())
    } else {
        Err(format!(
            "copy-project response does not identify its source and target; got {value}"
        ))
    }
}

fn validate_attachment_response(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("attachment response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("fileName").and_then(Value::as_str) == Some("ab-attachment.txt")
        && value.get("relativePath").and_then(Value::as_str)
            == Some(".attachments/ab-attachment.txt")
    {
        Ok(())
    } else {
        Err(format!(
            "attachment response does not match its requested name; got {value}"
        ))
    }
}

fn validate_project_delete(body: &[u8], project_id: &str) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("delete-project response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(true)
        && value.get("projectId").and_then(Value::as_str) == Some(project_id)
        && value
            .get("failedDirectories")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    {
        Ok(())
    } else {
        Err(format!(
            "delete-project did not confirm a clean deletion; got {value}"
        ))
    }
}

fn validate_download_archive(body: &[u8], user_id: &str, cid: &str) -> Result<(), String> {
    let entries = zip_semantic_entries(body).map_err(|error| error.to_string())?;
    let entries = entries
        .as_object()
        .ok_or_else(|| "ZIP semantic entries are not an object".to_string())?;
    let prefix = format!("{user_id}_{cid}/");
    if !entries.keys().any(|path| path.starts_with(&prefix))
        || entries.keys().any(|path| path.contains("/."))
        || !entries.contains_key(&format!("{prefix}README.md"))
    {
        return Err(format!(
            "download-all-files ZIP must have the expected prefix, README and no dot-segment entries; paths={:?}",
            entries.keys().collect::<Vec<_>>()
        ));
    }
    Ok(())
}

fn validate_workspace_archive(body: &[u8]) -> Result<(), String> {
    let entries = zip_semantic_entries(body).map_err(|error| error.to_string())?;
    let entries = entries
        .as_object()
        .ok_or_else(|| "ZIP semantic entries are not an object".to_string())?;
    if !entries.contains_key("README.md")
        || !entries.contains_key(".gitignore")
        || entries.keys().any(|path| path.starts_with("empty/"))
        || !entries.contains_key(".hidden.txt")
    {
        return Err(format!(
            "zip-workspace ZIP must be unprefixed, retain hidden and gitignore files, and exclude the requested directory; paths={:?}",
            entries.keys().collect::<Vec<_>>()
        ));
    }
    Ok(())
}

fn git_scenario(
    name: impl Into<String>,
    project_id: &str,
    spec: RequestSpec,
    preparation: Option<GitPreparation>,
) -> GitScenario {
    GitScenario {
        name: name.into(),
        project_id: project_id.to_string(),
        spec,
        preparation,
    }
}

fn git_get(project_id: &str, route: &str) -> RequestSpec {
    get_spec(format!(
        "/api/git/{route}?workspaceType=pageApp&projectId={project_id}"
    ))
}

fn git_json(project_id: &str, method: Method, path: &str, mut body: Value) -> Result<RequestSpec> {
    let object = body
        .as_object_mut()
        .context("Git A/B request body must be an object")?;
    object.insert("workspaceType".to_string(), json!("pageApp"));
    object.insert("projectId".to_string(), json!(project_id));
    json_spec(method, path, body)
}

fn append_git_seed(
    scenarios: &mut Vec<GitScenario>,
    project_id: &str,
    prefix: &str,
    second_commit: bool,
) -> Result<()> {
    scenarios.push(git_scenario(
        format!("{prefix}-init"),
        project_id,
        git_json(project_id, Method::POST, "/api/git/init", json!({}))?,
        None,
    ));
    scenarios.push(git_scenario(
        format!("{prefix}-add-initial"),
        project_id,
        git_json(
            project_id,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md","src/index.html"]}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        format!("{prefix}-commit-initial"),
        project_id,
        git_json(
            project_id,
            Method::POST,
            "/api/git/commit",
            json!({
                "message":"A/B initial commit",
                "authorName":"File Server A-B",
                "authorEmail":"ab@example.invalid"
            }),
        )?,
        None,
    ));
    if second_commit {
        scenarios.push(git_scenario(
            format!("{prefix}-add-second"),
            project_id,
            git_json(
                project_id,
                Method::POST,
                "/api/git/add",
                json!({"files":["README.md","src/new.txt"]}),
            )?,
            Some(GitPreparation::SecondCommitChange),
        ));
        scenarios.push(git_scenario(
            format!("{prefix}-commit-second"),
            project_id,
            git_json(
                project_id,
                Method::POST,
                "/api/git/commit",
                json!({
                    "message":"A/B second commit",
                    "authorName":"File Server A-B",
                    "authorEmail":"ab@example.invalid"
                }),
            )?,
            None,
        ));
    }
    Ok(())
}

fn git_scenarios() -> Result<Vec<GitScenario>> {
    let main = GIT_PROJECT_MAIN;
    let mut scenarios = vec![
        git_scenario(
            "git-init",
            main,
            git_json(main, Method::POST, "/api/git/init", json!({}))?,
            None,
        ),
        git_scenario(
            "git-status-before-first-commit",
            main,
            git_get(main, "status"),
            None,
        ),
        git_scenario(
            "git-add-initial-files",
            main,
            git_json(
                main,
                Method::POST,
                "/api/git/add",
                json!({"files":["README.md","src/index.html"]}),
            )?,
            None,
        ),
        git_scenario(
            "git-commit-initial",
            main,
            git_json(
                main,
                Method::POST,
                "/api/git/commit",
                json!({
                    "message":"A/B initial commit",
                    "authorName":"File Server A-B",
                    "authorEmail":"ab@example.invalid"
                }),
            )?,
            None,
        ),
        git_scenario("git-status-clean", main, git_get(main, "status"), None),
        git_scenario(
            "git-read-head-file",
            main,
            git_json(
                main,
                Method::POST,
                "/api/git/file-content",
                json!({"filePath":"src/index.html","ref":"HEAD"}),
            )?,
            None,
        ),
    ];

    let branch_list = git_get(main, "branches");
    scenarios.push(git_scenario(
        "git-create-branch",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/branch-create",
            json!({"branchName":"ab-review"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-list-branches-after-create",
        main,
        branch_list,
        None,
    ));
    scenarios.push(git_scenario(
        "git-switch-main",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/branch-switch",
            json!({"branchName":"main"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-create-tag",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/tag-create",
            json!({"tagName":"ab-v1","message":"A/B baseline tag"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-list-tags",
        main,
        git_get(main, "tags"),
        None,
    ));
    scenarios.push(git_scenario(
        "git-delete-tag",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/tag-delete",
            json!({"tagName":"ab-v1"}),
        )?,
        None,
    ));

    scenarios.push(git_scenario(
        "git-status-after-worktree-change",
        main,
        git_get(main, "status"),
        Some(GitPreparation::MainWorktreeChange),
    ));
    scenarios.push(git_scenario(
        "git-worktree-diff",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/diff",
            json!({"source":"worktree"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-add-second-change",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md","src/new.txt"]}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-staged-diff",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/diff",
            json!({"source":"staged"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-unstage-new-file",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/unstage",
            json!({"files":["src/new.txt"]}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-status-after-unstage",
        main,
        git_get(main, "status"),
        None,
    ));
    scenarios.push(git_scenario(
        "git-restage-second-change",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md","src/new.txt"]}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-commit-second",
        main,
        git_json(
            main,
            Method::POST,
            "/api/git/commit",
            json!({
                "message":"A/B second commit",
                "authorName":"File Server A-B",
                "authorEmail":"ab@example.invalid"
            }),
        )?,
        None,
    ));
    let mut git_log = get_spec(format!(
        "/api/git/log?workspaceType=pageApp&projectId={main}&maxCount=10"
    ));
    git_log.normalized_paths = vec![
        "/commits/0/hash".into(),
        "/commits/0/date".into(),
        "/commits/1/hash".into(),
        "/commits/1/date".into(),
    ];
    scenarios.push(git_scenario("git-log", main, git_log, None));

    let branch_delete = GIT_PROJECT_BRANCH_DELETE;
    append_git_seed(
        &mut scenarios,
        branch_delete,
        "git-branch-delete-seed",
        false,
    )?;
    scenarios.push(git_scenario(
        "git-delete-branch",
        branch_delete,
        git_json(
            branch_delete,
            Method::POST,
            "/api/git/branch-delete",
            json!({"branchName":"ab-review"}),
        )?,
        Some(GitPreparation::CreateDeleteBranch),
    ));
    scenarios.push(git_scenario(
        "git-list-branches-after-delete",
        branch_delete,
        git_get(branch_delete, "branches"),
        None,
    ));

    let revert = GIT_PROJECT_REVERT;
    append_git_seed(&mut scenarios, revert, "git-revert-seed", true)?;
    let mut revert_spec = git_json(
        revert,
        Method::POST,
        "/api/git/revert",
        json!({
            "target":"HEAD~1",
            "message":"A/B revert to initial tree",
            "authorName":"File Server A-B",
            "authorEmail":"ab@example.invalid"
        }),
    )?;
    revert_spec.normalized_paths = vec!["/commit".into(), "/target".into(), "/previousHead".into()];
    scenarios.push(git_scenario(
        "git-revert-to-initial",
        revert,
        revert_spec,
        None,
    ));

    let reset_mixed = GIT_PROJECT_RESET_MIXED;
    append_git_seed(&mut scenarios, reset_mixed, "git-reset-mixed-seed", true)?;
    let mut reset_mixed_spec = git_json(
        reset_mixed,
        Method::POST,
        "/api/git/reset",
        json!({"target":"HEAD~1","mode":"mixed"}),
    )?;
    reset_mixed_spec.normalized_paths = vec!["/previousHead".into()];
    scenarios.push(git_scenario(
        "git-reset-mixed-to-first",
        reset_mixed,
        reset_mixed_spec,
        None,
    ));

    let reset_hard = GIT_PROJECT_RESET_HARD;
    append_git_seed(&mut scenarios, reset_hard, "git-reset-hard-seed", true)?;
    let mut reset_hard_spec = git_json(
        reset_hard,
        Method::POST,
        "/api/git/reset",
        json!({"target":"HEAD~1","mode":"hard"}),
    )?;
    reset_hard_spec.normalized_paths = vec!["/previousHead".into()];
    scenarios.push(git_scenario(
        "git-reset-hard-to-first",
        reset_hard,
        reset_hard_spec,
        Some(GitPreparation::ResetHardDirty),
    ));

    let reset_soft = GIT_PROJECT_RESET_SOFT;
    append_git_seed(&mut scenarios, reset_soft, "git-reset-soft-seed", true)?;
    let mut reset_soft_spec = git_json(
        reset_soft,
        Method::POST,
        "/api/git/reset",
        json!({"target":"HEAD~1","mode":"soft"}),
    )?;
    reset_soft_spec.normalized_paths = vec!["/previousHead".into()];
    scenarios.push(git_scenario(
        "git-reset-soft-to-first",
        reset_soft,
        reset_soft_spec,
        None,
    ));

    let checkout = GIT_PROJECT_CHECKOUT;
    append_git_seed(&mut scenarios, checkout, "git-checkout-seed", false)?;
    scenarios.push(git_scenario(
        "git-checkout-head",
        checkout,
        git_json(
            checkout,
            Method::POST,
            "/api/git/checkout",
            json!({"target":"HEAD"}),
        )?,
        Some(GitPreparation::CheckoutDirty),
    ));
    scenarios.push(git_scenario(
        "git-status-after-checkout",
        checkout,
        git_get(checkout, "status"),
        None,
    ));

    let discard = GIT_PROJECT_DISCARD;
    append_git_seed(&mut scenarios, discard, "git-discard-seed", false)?;
    scenarios.push(git_scenario(
        "git-discard-all",
        discard,
        git_json(
            discard,
            Method::POST,
            "/api/git/discard",
            json!({"files":[]}),
        )?,
        Some(GitPreparation::DiscardDirty),
    ));

    let merge_conflict = GIT_PROJECT_MERGE_CONFLICT;
    append_git_seed(
        &mut scenarios,
        merge_conflict,
        "git-merge-conflict-seed",
        false,
    )?;
    scenarios.push(git_scenario(
        "git-merge-conflict-create-feature",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/branch-create",
            json!({"branchName":"ab-conflict-feature"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-switch-feature",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/branch-switch",
            json!({"branchName":"ab-conflict-feature"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-stage-feature",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md"]}),
        )?,
        Some(GitPreparation::MergeConflictFeatureChange),
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-commit-feature",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/commit",
            json!({
                "message":"A/B conflict feature commit",
                "authorName":"File Server A-B",
                "authorEmail":"ab@example.invalid"
            }),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-switch-main",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/branch-switch",
            json!({"branchName":"main"}),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-stage-main",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/add",
            json!({"files":["README.md"]}),
        )?,
        Some(GitPreparation::MergeConflictMainChange),
    ));
    scenarios.push(git_scenario(
        "git-merge-conflict-commit-main",
        merge_conflict,
        git_json(
            merge_conflict,
            Method::POST,
            "/api/git/commit",
            json!({
                "message":"A/B conflict main commit",
                "authorName":"File Server A-B",
                "authorEmail":"ab@example.invalid"
            }),
        )?,
        None,
    ));
    scenarios.push(git_scenario(
        "git-status-after-merge-conflict",
        merge_conflict,
        git_get(merge_conflict, "status"),
        Some(GitPreparation::CreateMergeConflict),
    ));

    Ok(scenarios)
}

fn json_spec(method: Method, path: &str, body: Value) -> Result<RequestSpec> {
    Ok(RequestSpec {
        method,
        path: path.to_string(),
        body: serde_json::to_vec(&body).context("serialize A/B request body")?,
        content_type: Some("application/json"),
        headers: BTreeMap::new(),
        health_probe: false,
        expected_status: ExpectedStatus::Success2xx,
        normalized_paths: if path == "/api/git/commit" {
            vec!["/commit".to_string()]
        } else {
            Vec::new()
        },
        normalized_headers: Vec::new(),
        timeout: Duration::from_secs(30),
    })
}

fn project_create_spec(project_id: &str, template_type: &str) -> Result<RequestSpec> {
    let mut spec = json_spec(
        Method::POST,
        "/api/project/create-project",
        json!({"projectId": project_id, "templateType": template_type}),
    )?;
    // Template initialization includes extraction, agent metadata synchronization, and a Git
    // commit. Cold Docker volumes can exceed the generic API timeout even when initialization
    // succeeds; allow the comparison to observe the actual result rather than cascade into
    // later scenarios against a project that is still being initialized.
    spec.timeout = Duration::from_secs(120);
    Ok(spec)
}

fn get_spec(path: impl Into<String>) -> RequestSpec {
    RequestSpec {
        method: Method::GET,
        path: path.into(),
        body: Vec::new(),
        content_type: None,
        headers: BTreeMap::new(),
        health_probe: false,
        expected_status: ExpectedStatus::Success2xx,
        normalized_paths: Vec::new(),
        normalized_headers: Vec::new(),
        timeout: Duration::from_secs(30),
    }
}

fn prepare_git_fixtures(root: &Path) -> Result<()> {
    for project_id in GIT_PROJECT_IDS {
        let project = root.join("project-workspace").join(project_id);
        fs::create_dir_all(project.join("src"))
            .with_context(|| format!("create Git fixture directory {}", project.display()))?;
        fs::write(project.join("README.md"), "Git A/B fixture\n")
            .with_context(|| format!("write Git fixture README in {}", project.display()))?;
        fs::write(project.join("src/index.html"), "<main>Git fixture</main>\n")
            .with_context(|| format!("write Git fixture entry in {}", project.display()))?;
    }
    Ok(())
}

fn prepare_git_scenario(
    preparation: GitPreparation,
    project_id: &str,
    rust_root: &Path,
    ts_root: &Path,
) -> Result<()> {
    for root in [rust_root, ts_root] {
        let project = root.join("project-workspace").join(project_id);
        match preparation {
            GitPreparation::MainWorktreeChange | GitPreparation::SecondCommitChange => {
                fs::write(project.join("README.md"), "Git A/B fixture changed\n")?;
                fs::write(project.join("src/new.txt"), "new staged fixture\n")?;
            }
            GitPreparation::CheckoutDirty => {
                fs::write(project.join("README.md"), "dirty before checkout\n")?;
                fs::write(
                    project.join("checkout-extra.txt"),
                    "checkout keeps unrelated file\n",
                )?;
            }
            GitPreparation::DiscardDirty => {
                fs::write(project.join("README.md"), "dirty before discard\n")?;
                fs::write(
                    project.join("discard-extra.txt"),
                    "discard removes untracked file\n",
                )?;
            }
            GitPreparation::ResetHardDirty => {
                fs::write(project.join("README.md"), "dirty before hard reset\n")?;
                fs::write(
                    project.join("reset-extra.txt"),
                    "hard reset removes untracked file\n",
                )?;
            }
            GitPreparation::CreateDeleteBranch => {
                run_git_fixture_command(&project, &["branch", "ab-review"])?;
            }
            GitPreparation::MergeConflictFeatureChange => {
                fs::write(project.join("README.md"), "feature branch version\n")?;
            }
            GitPreparation::MergeConflictMainChange => {
                fs::write(project.join("README.md"), "main branch version\n")?;
            }
            GitPreparation::CreateMergeConflict => {
                run_git_merge_conflict_fixture(&project, "ab-conflict-feature", "README.md")?;
            }
        }
    }
    Ok(())
}

fn run_git_merge_conflict_fixture(project: &Path, branch: &str, path: &str) -> Result<()> {
    let merge = ProcessCommand::new("git")
        .arg("-C")
        .arg(project)
        .args([
            "-c",
            "user.name=File Server A-B",
            "-c",
            "user.email=ab@example.invalid",
            "merge",
            "--no-commit",
            "--no-ff",
            branch,
        ])
        .output()
        .with_context(|| format!("start Git merge fixture in {}", project.display()))?;
    if merge.status.code() != Some(1) {
        bail!(
            "expected a merge conflict for {path} in {} but git merge exited {}: {}",
            project.display(),
            merge.status,
            String::from_utf8_lossy(&merge.stderr).trim()
        );
    }

    let unmerged = ProcessCommand::new("git")
        .arg("-C")
        .arg(project)
        .args(["ls-files", "-u", "--", path])
        .output()
        .with_context(|| format!("inspect unmerged Git index in {}", project.display()))?;
    if !unmerged.status.success()
        || !String::from_utf8_lossy(&unmerged.stdout)
            .lines()
            .any(|line| line.ends_with(path))
    {
        bail!(
            "Git merge returned conflict status but did not leave {path} unmerged in {}: {}",
            project.display(),
            String::from_utf8_lossy(&unmerged.stderr).trim()
        );
    }
    Ok(())
}

fn run_git_fixture_command(project: &Path, args: &[&str]) -> Result<()> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .with_context(|| {
            format!(
                "start git {} for fixture {}",
                args.join(" "),
                project.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "git {} failed for fixture {} with {}: {}",
            args.join(" "),
            project.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[derive(Debug)]
struct DevServerIdentity {
    pid: u64,
    port: u16,
}

fn react_log_stages() -> Vec<String> {
    [
        "build-react-get-dev-log",
        "build-react-log-cache-stats",
        "build-react-clear-log-cache",
        "build-react-log-cache-stats-after-clear",
        "build-react-port-pool-status",
    ]
    .map(str::to_string)
    .to_vec()
}

fn lifecycle_tail(template_type: &str) -> Vec<String> {
    [
        format!("build-{template_type}-keep-alive"),
        format!("build-{template_type}-restart-dev"),
        format!("build-{template_type}-restarted-dev-http-reachable"),
        format!("build-{template_type}-stop-dev"),
        format!("build-{template_type}-stop-dev-port-unreachable"),
        format!("build-{template_type}-list-after-stop"),
    ]
    .to_vec()
}

fn stages_after_build(template_type: &str) -> Vec<String> {
    let mut stages = vec![
        format!("build-{template_type}-static-dist-index"),
        format!("build-{template_type}-start-dev"),
    ];
    stages.extend(stages_after_start(template_type));
    stages
}

fn stages_after_start(template_type: &str) -> Vec<String> {
    let mut stages = vec![format!("build-{template_type}-dev-http-reachable")];
    if template_type == "react" {
        stages.extend(react_log_stages());
    }
    stages.extend(lifecycle_tail(template_type));
    stages
}

fn stages_after_probe(template_type: &str) -> Vec<String> {
    stages_after_start(template_type)
}

fn stages_after_log_stats() -> Vec<String> {
    vec![
        "build-react-clear-log-cache".to_string(),
        "build-react-log-cache-stats-after-clear".to_string(),
    ]
}

fn stages_after_restart(template_type: &str) -> Vec<String> {
    [
        format!("build-{template_type}-restarted-dev-http-reachable"),
        format!("build-{template_type}-stop-dev"),
        format!("build-{template_type}-stop-dev-port-unreachable"),
        format!("build-{template_type}-list-after-stop"),
    ]
    .to_vec()
}

fn stages_after_stop(template_type: &str) -> Vec<String> {
    stages_after_restart(template_type)
}

/// Record the given stage names as blocked, in dependency order. Stages that were
/// already recorded (executed or blocked) are skipped, so chains can be listed
/// redundantly after partial progress.
fn record_blocked_stages(
    journal: &RequestJournal,
    recorder: &mut Recorder,
    stages: &[String],
) -> Result<()> {
    for stage in stages {
        if recorder.cases.iter().any(|case| case.case == *stage) {
            continue;
        }
        let Some(blocker) = find_blocker(&recorder.cases, case_dependencies(stage)) else {
            continue;
        };
        recorder.record(journal, blocked_case(stage, &blocker))?;
        println!("BLKD {stage} (blocked by {blocker})");
    }
    Ok(())
}

/// Run one paired build-suite stage unless a dependency already failed or was blocked.
/// The stage is recorded as blocked in that case; otherwise the caller owns recording
/// the returned case after applying its scenario assertions.
#[allow(clippy::too_many_arguments)]
async fn run_gated_pair(
    client: &Client,
    journal: &mut RequestJournal,
    recorder: &mut Recorder,
    case: &str,
    rust_url: &str,
    ts_url: &str,
    rust_spec: &RequestSpec,
    ts_spec: &RequestSpec,
    target: RequestTarget,
) -> Result<Option<(CaseResult, Exchange, Exchange)>> {
    if let Some(blocker) = find_blocker(&recorder.cases, case_dependencies(case)) {
        recorder.record(journal, blocked_case(case, &blocker))?;
        println!("BLKD {case} (blocked by {blocker})");
        return Ok(None);
    }
    let (case, rust, ts) = run_pair_specs(
        client, journal, case, rust_url, ts_url, rust_spec, ts_spec, target,
    )
    .await?;
    Ok(Some((case, rust, ts)))
}

/// Best-effort stop of a dev server that a failed comparison branch would otherwise
/// leak. The request is journaled under a distinct cleanup case name so it is visible
/// as evidence without claiming the stop-dev route comparison was executed.
async fn stop_dev_cleanup(
    client: &Client,
    journal: &mut RequestJournal,
    api_url: &str,
    case: &str,
    project_id: &str,
    side: &str,
    server: &DevServerIdentity,
) {
    let mut spec = get_spec(format!(
        "/api/build/stop-dev?projectId={project_id}&pid={}",
        server.pid
    ));
    spec.timeout = Duration::from_secs(120);
    match exchange(
        client,
        journal,
        case,
        side,
        api_url,
        &spec,
        RequestTarget::Api,
    )
    .await
    {
        Ok(stopped) => println!(
            "CLEANUP {case}/{side}: HTTP {}",
            stopped.status.map(|status| status.as_u16()).unwrap_or(0)
        ),
        Err(error) => eprintln!("file-server-ab: cleanup stop {case}/{side} failed: {error:#}"),
    }
}

async fn run_build_suite(
    client: &Client,
    journal: &mut RequestJournal,
    recorder: &mut Recorder,
    rust_api_url: &str,
    ts_api_url: &str,
    rust_dev_url: &str,
    ts_dev_url: &str,
) -> Result<()> {
    let parse_case = "build-parse-error";
    let parse_spec = json_spec(
        Method::POST,
        "/api/build/parse-build-error",
        json!({
            "projectId": "file-server-ab-build-react",
            "errorMessage": "Error: Cannot find module 'react-dom'"
        }),
    )?;
    let (mut parse_result, rust_parse, ts_parse) = run_pair_specs(
        client,
        journal,
        parse_case,
        rust_api_url,
        ts_api_url,
        &parse_spec,
        &parse_spec,
        RequestTarget::Api,
    )
    .await?;
    for (side, response) in [("rust", &rust_parse), ("typescript", &ts_parse)] {
        let value = serde_json::from_slice::<Value>(&response.body).ok();
        if !json_bool(&response.body, "success")
            || !value
                .as_ref()
                .and_then(|body| body.get("message"))
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("react-dom"))
        {
            parse_result.differences.push(assertion_difference(
                parse_case,
                &format!("/assertions/{side}/parsed-message"),
                "expected success=true and an explanation containing the missing dependency".into(),
            ));
        }
    }
    println!(
        "{} {}",
        if parse_result.differences.is_empty() {
            "PASS"
        } else {
            "DIFF"
        },
        parse_result.case
    );
    recorder.record(journal, parse_result)?;

    for (project_id, template_type) in [
        ("file-server-ab-build-react", "react"),
        ("file-server-ab-build-vue", "vue3"),
    ] {
        let create_case = format!("build-{template_type}-create-project");
        let mut create_spec = json_spec(
            Method::POST,
            "/api/project/create-project",
            json!({"projectId":project_id,"templateType":template_type}),
        )?;
        // Project creation includes extracting the template and creating its initial Git
        // commit. On a cold Docker bind mount this can take longer than the generic 30s API
        // timeout; let the comparison observe the actual result instead of cascading into
        // build requests against a project that is still being initialized.
        create_spec.timeout = Duration::from_secs(120);
        let (case, _, _) = run_pair_specs(
            client,
            journal,
            &create_case,
            rust_api_url,
            ts_api_url,
            &create_spec,
            &create_spec,
            RequestTarget::Api,
        )
        .await?;
        println!("{} {}", if case.equal { "PASS" } else { "DIFF" }, case.case);
        recorder.record(journal, case)?;

        let build_case = format!("build-{template_type}-production-build");
        let mut build_spec = get_spec(format!(
            "/api/build/build?projectId={project_id}&basePath=%2F"
        ));
        build_spec.timeout = Duration::from_secs(720);
        let Some((case, _, _)) = run_gated_pair(
            client,
            journal,
            recorder,
            &build_case,
            rust_api_url,
            ts_api_url,
            &build_spec,
            &build_spec,
            RequestTarget::Api,
        )
        .await?
        else {
            record_blocked_stages(journal, recorder, &stages_after_build(template_type))?;
            continue;
        };
        println!("{} {}", if case.equal { "PASS" } else { "DIFF" }, case.case);
        recorder.record(journal, case)?;

        let artifact_case = format!("build-{template_type}-static-dist-index");
        let artifact_spec = get_spec(format!("/api/page/static/{project_id}/dist/index.html"));
        // 同上: header 协议语义层负责 etag/last-modified 的校验。
        let Some((case, _, _)) = run_gated_pair(
            client,
            journal,
            recorder,
            &artifact_case,
            rust_api_url,
            ts_api_url,
            &artifact_spec,
            &artifact_spec,
            RequestTarget::Api,
        )
        .await?
        else {
            record_blocked_stages(journal, recorder, &stages_after_build(template_type))?;
            continue;
        };
        println!("{} {}", if case.equal { "PASS" } else { "DIFF" }, case.case);
        recorder.record(journal, case)?;

        let start_case = format!("build-{template_type}-start-dev");
        let mut start_spec = get_spec(format!(
            "/api/build/start-dev?projectId={project_id}&basePath=%2F"
        ));
        start_spec.normalized_paths = vec!["/pid".into(), "/port".into()];
        start_spec.timeout = Duration::from_secs(720);
        let Some((mut case, rust_start, ts_start)) = run_gated_pair(
            client,
            journal,
            recorder,
            &start_case,
            rust_api_url,
            ts_api_url,
            &start_spec,
            &start_spec,
            RequestTarget::Api,
        )
        .await?
        else {
            record_blocked_stages(journal, recorder, &stages_after_start(template_type))?;
            continue;
        };
        let rust_server = parse_dev_server(&rust_start, "rust", project_id);
        let ts_server = parse_dev_server(&ts_start, "typescript", project_id);
        match (rust_server, ts_server) {
            (Ok(rust_server), Ok(ts_server)) => {
                if !(4000..=55_000).contains(&rust_server.port)
                    || !(4000..=55_000).contains(&ts_server.port)
                {
                    case.differences.push(assertion_difference(
                        &start_case,
                        "/assertions/dev-port",
                        format!(
                            "expected both dev-server ports in 4000-55000; Rust={}, TypeScript={}",
                            rust_server.port, ts_server.port
                        ),
                    ));
                }
                println!(
                    "{} {}",
                    if case.differences.is_empty() {
                        "PASS"
                    } else {
                        "DIFF"
                    },
                    case.case
                );
                recorder.record(journal, case)?;

                let probe_case = format!("build-{template_type}-dev-http-reachable");
                let mut probe_spec = get_spec("/".to_string());
                // Vite's default host check rejects Compose DNS names (rust/typescript).
                // Preserve the direct HTTP probe while presenting its allowed local Host.
                probe_spec.headers.insert("host".into(), "localhost".into());
                let rust_dev_endpoint = format!(
                    "{}:{}",
                    rust_dev_url.trim_end_matches('/'),
                    rust_server.port
                );
                let ts_dev_endpoint =
                    format!("{}:{}", ts_dev_url.trim_end_matches('/'), ts_server.port);
                let Some((case, _, _)) = run_gated_pair(
                    client,
                    journal,
                    recorder,
                    &probe_case,
                    &rust_dev_endpoint,
                    &ts_dev_endpoint,
                    &probe_spec,
                    &probe_spec,
                    RequestTarget::DevServer,
                )
                .await?
                else {
                    // The dev servers stay up for now; a later restart/stop may still run.
                    record_blocked_stages(journal, recorder, &stages_after_probe(template_type))?;
                    stop_dev_cleanup(
                        client,
                        journal,
                        rust_api_url,
                        &format!("build-{template_type}-stop-dev-cleanup"),
                        project_id,
                        "rust",
                        &rust_server,
                    )
                    .await;
                    stop_dev_cleanup(
                        client,
                        journal,
                        ts_api_url,
                        &format!("build-{template_type}-stop-dev-cleanup"),
                        project_id,
                        "typescript",
                        &ts_server,
                    )
                    .await;
                    continue;
                };
                println!("{} {}", if case.equal { "PASS" } else { "DIFF" }, case.case);
                recorder.record(journal, case)?;

                if template_type == "react" {
                    let log_case = "build-react-get-dev-log";
                    let mut log_spec = get_spec(format!(
                        "/api/build/get-dev-log?projectId={project_id}&startIndex=1&logType=temp"
                    ));
                    // Dev-log text is each implementation's own install/vite
                    // instrumentation (structured pnpm events vs raw output, different
                    // volumes), so line content, count, and totalLines cannot be equal
                    // across implementations. The page contract is asserted independently
                    // below (non-empty page, line numbering, totalLines consistency);
                    // normalize the dynamic log text, its generated file name, the
                    // line-volume-derived totalLines, and the body-derived length/etag
                    // headers while keeping success/startIndex compared exactly.
                    log_spec.normalized_paths =
                        vec!["/logs".into(), "/logFileName".into(), "/totalLines".into()];
                    log_spec.normalized_headers = vec!["content-length".into(), "etag".into()];
                    let Some((mut log_result, rust_log, ts_log)) = run_gated_pair(
                        client,
                        journal,
                        recorder,
                        log_case,
                        rust_api_url,
                        ts_api_url,
                        &log_spec,
                        &log_spec,
                        RequestTarget::Api,
                    )
                    .await?
                    else {
                        record_blocked_stages(journal, recorder, &stages_after_probe("react"))?;
                        stop_dev_cleanup(
                            client,
                            journal,
                            rust_api_url,
                            "build-react-stop-dev-cleanup",
                            project_id,
                            "rust",
                            &rust_server,
                        )
                        .await;
                        stop_dev_cleanup(
                            client,
                            journal,
                            ts_api_url,
                            "build-react-stop-dev-cleanup",
                            project_id,
                            "typescript",
                            &ts_server,
                        )
                        .await;
                        continue;
                    };
                    let mut page1_last_line = BTreeMap::new();
                    for (side, response) in [("rust", &rust_log), ("typescript", &ts_log)] {
                        match validate_log_page(&response.body, 1) {
                            Ok((last_line, _total)) => {
                                page1_last_line.insert(side.to_string(), last_line);
                            }
                            Err(error) => {
                                log_result.differences.push(assertion_difference(
                                    log_case,
                                    &format!("/assertions/{side}/log-page"),
                                    error,
                                ));
                            }
                        }
                    }
                    println!(
                        "{} {}",
                        if log_result.differences.is_empty() {
                            "PASS"
                        } else {
                            "DIFF"
                        },
                        log_result.case
                    );
                    recorder.record(journal, log_result)?;

                    // Page-2 query at each side's own next line proves the paging has
                    // no overlap or gap: the second page's lines must run consecutively
                    // from the first page's last line + 1. The log may grow between the
                    // two queries, so totalLines equality across pages is not required.
                    let page2_case = "build-react-get-dev-log-page-2";
                    if let Some(blocker) =
                        find_blocker(&recorder.cases, case_dependencies(page2_case))
                    {
                        recorder.record(journal, blocked_case(page2_case, &blocker))?;
                        println!("BLKD {page2_case} (blocked by {blocker})");
                    } else if page1_last_line.len() == 2 {
                        let mut rust_page2 = get_spec(format!(
                            "/api/build/get-dev-log?projectId={project_id}&startIndex={}&logType=temp",
                            page1_last_line["rust"] + 1
                        ));
                        let mut ts_page2 = get_spec(format!(
                            "/api/build/get-dev-log?projectId={project_id}&startIndex={}&logType=temp",
                            page1_last_line["typescript"] + 1
                        ));
                        for spec in [&mut rust_page2, &mut ts_page2] {
                            // startIndex 是各侧回显自己请求的行号 (两侧行数不同),
                            // 跨侧比较无意义; 回显正确性由 validate_log_page 断言。
                            spec.normalized_paths = vec![
                                "/logs".into(),
                                "/logFileName".into(),
                                "/totalLines".into(),
                                "/startIndex".into(),
                            ];
                            spec.normalized_headers = vec!["content-length".into(), "etag".into()];
                        }
                        let (mut page2_result, rust_page2_response, ts_page2_response) =
                            run_pair_specs(
                                client,
                                journal,
                                page2_case,
                                rust_api_url,
                                ts_api_url,
                                &rust_page2,
                                &ts_page2,
                                RequestTarget::Api,
                            )
                            .await?;
                        for (side, response, next_start) in [
                            ("rust", &rust_page2_response, page1_last_line["rust"] + 1),
                            (
                                "typescript",
                                &ts_page2_response,
                                page1_last_line["typescript"] + 1,
                            ),
                        ] {
                            if let Err(error) = validate_log_page(&response.body, next_start) {
                                page2_result.differences.push(assertion_difference(
                                    page2_case,
                                    &format!("/assertions/{side}/log-page-2"),
                                    error,
                                ));
                            }
                        }
                        println!(
                            "{} {}",
                            if page2_result.differences.is_empty() {
                                "PASS"
                            } else {
                                "DIFF"
                            },
                            page2_result.case
                        );
                        recorder.record(journal, page2_result)?;
                    } else {
                        // Page 1 already failed its contract on at least one side; a
                        // paging query against an unreadable first page is noise.
                        let blocker = log_case;
                        recorder.record(journal, blocked_case(page2_case, blocker))?;
                        println!("BLKD {page2_case} (blocked by {blocker})");
                    }

                    let stats_case = "build-react-log-cache-stats";
                    let mut stats_spec = get_spec("/api/build/get-log-cache-stats");
                    // maxFileSizeMB/totalCacheSizeMB 由被缓存的 dev 日志体量派生
                    // (两侧 instrumentation 体量不同, 与 /logs 归一同类), 按形状归一;
                    // cacheSize 等结构字段仍逐字比较。
                    stats_spec.normalized_paths = vec![
                        "/stats/maxFileSizeMB".into(),
                        "/stats/totalCacheSizeMB".into(),
                    ];
                    let Some((mut stats_result, rust_stats, ts_stats)) = run_gated_pair(
                        client,
                        journal,
                        recorder,
                        stats_case,
                        rust_api_url,
                        ts_api_url,
                        &stats_spec,
                        &stats_spec,
                        RequestTarget::Api,
                    )
                    .await?
                    else {
                        record_blocked_stages(journal, recorder, &stages_after_log_stats())?;
                        stop_dev_cleanup(
                            client,
                            journal,
                            rust_api_url,
                            "build-react-stop-dev-cleanup",
                            project_id,
                            "rust",
                            &rust_server,
                        )
                        .await;
                        stop_dev_cleanup(
                            client,
                            journal,
                            ts_api_url,
                            "build-react-stop-dev-cleanup",
                            project_id,
                            "typescript",
                            &ts_server,
                        )
                        .await;
                        continue;
                    };
                    for (side, response) in [("rust", &rust_stats), ("typescript", &ts_stats)] {
                        let value = serde_json::from_slice::<Value>(&response.body).ok();
                        if !json_bool(&response.body, "success")
                            || value
                                .as_ref()
                                .and_then(|body| body.get("stats"))
                                .and_then(Value::as_object)
                                .is_none()
                        {
                            stats_result.differences.push(assertion_difference(
                                stats_case,
                                &format!("/assertions/{side}/stats"),
                                "expected success=true and a stats object".into(),
                            ));
                        }
                    }
                    println!(
                        "{} {}",
                        if stats_result.differences.is_empty() {
                            "PASS"
                        } else {
                            "DIFF"
                        },
                        stats_result.case
                    );
                    recorder.record(journal, stats_result)?;

                    let clear_case = "build-react-clear-log-cache";
                    let clear_spec = get_spec("/api/build/clear-all-log-cache");
                    if let Some((mut clear_result, rust_clear, ts_clear)) = run_gated_pair(
                        client,
                        journal,
                        recorder,
                        clear_case,
                        rust_api_url,
                        ts_api_url,
                        &clear_spec,
                        &clear_spec,
                        RequestTarget::Api,
                    )
                    .await?
                    {
                        for (side, response) in [("rust", &rust_clear), ("typescript", &ts_clear)] {
                            if !json_bool(&response.body, "success") {
                                clear_result.differences.push(assertion_difference(
                                    clear_case,
                                    &format!("/assertions/{side}/success"),
                                    "expected success=true after clearing log cache".into(),
                                ));
                            }
                        }
                        println!(
                            "{} {}",
                            if clear_result.differences.is_empty() {
                                "PASS"
                            } else {
                                "DIFF"
                            },
                            clear_result.case
                        );
                        recorder.record(journal, clear_result)?;

                        let cleared_stats_case = "build-react-log-cache-stats-after-clear";
                        let cleared_stats_spec = get_spec("/api/build/get-log-cache-stats");
                        if let Some((
                            mut cleared_stats_result,
                            rust_cleared_stats,
                            ts_cleared_stats,
                        )) = run_gated_pair(
                            client,
                            journal,
                            recorder,
                            cleared_stats_case,
                            rust_api_url,
                            ts_api_url,
                            &cleared_stats_spec,
                            &cleared_stats_spec,
                            RequestTarget::Api,
                        )
                        .await?
                        {
                            for (side, response) in [
                                ("rust", &rust_cleared_stats),
                                ("typescript", &ts_cleared_stats),
                            ] {
                                let cache_size = serde_json::from_slice::<Value>(&response.body)
                                    .ok()
                                    .and_then(|body| body.get("stats").cloned())
                                    .and_then(|stats| stats.get("cacheSize").cloned())
                                    .and_then(|size| size.as_u64());
                                if !json_bool(&response.body, "success") || cache_size != Some(0) {
                                    cleared_stats_result.differences.push(assertion_difference(
                                        cleared_stats_case,
                                        &format!("/assertions/{side}/cache-cleared"),
                                        format!(
                                            "expected success=true and cacheSize=0, got {cache_size:?}"
                                        ),
                                    ));
                                }
                            }
                            println!(
                                "{} {}",
                                if cleared_stats_result.differences.is_empty() {
                                    "PASS"
                                } else {
                                    "DIFF"
                                },
                                cleared_stats_result.case
                            );
                            recorder.record(journal, cleared_stats_result)?;
                        }
                    } else {
                        record_blocked_stages(
                            journal,
                            recorder,
                            &["build-react-log-cache-stats-after-clear".to_string()],
                        )?;
                    }

                    let pool_case = "build-react-port-pool-status";
                    let mut pool_spec = get_spec("/api/build/port-pool-status");
                    // Both isolated services allocate different concrete ports. Validate each
                    // allocation against that side's start-dev response below, then compare
                    // the remaining port-pool contract normally.
                    pool_spec.normalized_paths = vec!["/allocations/0/port".into()];
                    if let Some((mut pool_result, rust_pool, ts_pool)) = run_gated_pair(
                        client,
                        journal,
                        recorder,
                        pool_case,
                        rust_api_url,
                        ts_api_url,
                        &pool_spec,
                        &pool_spec,
                        RequestTarget::Api,
                    )
                    .await?
                    {
                        for (side, response, expected_port) in [
                            ("rust", &rust_pool, rust_server.port),
                            ("typescript", &ts_pool, ts_server.port),
                        ] {
                            let value = serde_json::from_slice::<Value>(&response.body).ok();
                            let contains_running_project = value
                                .as_ref()
                                .and_then(|body| body.get("allocations"))
                                .and_then(Value::as_array)
                                .is_some_and(|allocations| {
                                    allocations.iter().any(|allocation| {
                                        allocation.get("projectId").and_then(Value::as_str)
                                            == Some(project_id)
                                            && allocation.get("port").and_then(Value::as_u64)
                                                == Some(u64::from(expected_port))
                                    })
                                });
                            if !json_bool(&response.body, "success") || !contains_running_project {
                                pool_result.differences.push(assertion_difference(
                                    pool_case,
                                    &format!("/assertions/{side}/allocation"),
                                    "expected the running project to be allocated its reported dev port".into(),
                                ));
                            }
                        }
                        println!(
                            "{} {}",
                            if pool_result.differences.is_empty() {
                                "PASS"
                            } else {
                                "DIFF"
                            },
                            pool_result.case
                        );
                        recorder.record(journal, pool_result)?;
                    }
                }

                let keep_case = format!("build-{template_type}-keep-alive");
                let mut rust_keep = get_spec(format!(
                    "/api/build/keep-alive?projectId={project_id}&pid={}&port={}&basePath=%2F",
                    rust_server.pid, rust_server.port
                ));
                let mut ts_keep = get_spec(format!(
                    "/api/build/keep-alive?projectId={project_id}&pid={}&port={}&basePath=%2F",
                    ts_server.pid, ts_server.port
                ));
                rust_keep.normalized_paths = vec!["/pid".into(), "/port".into()];
                ts_keep.normalized_paths = vec!["/pid".into(), "/port".into()];
                let Some((case, _, _)) = run_gated_pair(
                    client,
                    journal,
                    recorder,
                    &keep_case,
                    rust_api_url,
                    ts_api_url,
                    &rust_keep,
                    &ts_keep,
                    RequestTarget::Api,
                )
                .await?
                else {
                    record_blocked_stages(journal, recorder, &stages_after_restart(template_type))?;
                    stop_dev_cleanup(
                        client,
                        journal,
                        rust_api_url,
                        &format!("build-{template_type}-stop-dev-cleanup"),
                        project_id,
                        "rust",
                        &rust_server,
                    )
                    .await;
                    stop_dev_cleanup(
                        client,
                        journal,
                        ts_api_url,
                        &format!("build-{template_type}-stop-dev-cleanup"),
                        project_id,
                        "typescript",
                        &ts_server,
                    )
                    .await;
                    continue;
                };
                println!("{} {}", if case.equal { "PASS" } else { "DIFF" }, case.case);
                recorder.record(journal, case)?;

                let restart_case = format!("build-{template_type}-restart-dev");
                let mut restart_spec = get_spec(format!(
                    "/api/build/restart-dev?projectId={project_id}&basePath=%2F"
                ));
                restart_spec.normalized_paths = vec!["/pid".into(), "/port".into()];
                restart_spec.timeout = Duration::from_secs(720);
                let Some((mut case, rust_restart, ts_restart)) = run_gated_pair(
                    client,
                    journal,
                    recorder,
                    &restart_case,
                    rust_api_url,
                    ts_api_url,
                    &restart_spec,
                    &restart_spec,
                    RequestTarget::Api,
                )
                .await?
                else {
                    record_blocked_stages(journal, recorder, &stages_after_restart(template_type))?;
                    stop_dev_cleanup(
                        client,
                        journal,
                        rust_api_url,
                        &format!("build-{template_type}-stop-dev-cleanup"),
                        project_id,
                        "rust",
                        &rust_server,
                    )
                    .await;
                    stop_dev_cleanup(
                        client,
                        journal,
                        ts_api_url,
                        &format!("build-{template_type}-stop-dev-cleanup"),
                        project_id,
                        "typescript",
                        &ts_server,
                    )
                    .await;
                    continue;
                };
                let rust_server = parse_dev_server(&rust_restart, "rust", project_id);
                let ts_server = parse_dev_server(&ts_restart, "typescript", project_id);
                match (rust_server, ts_server) {
                    (Ok(rust_server), Ok(ts_server)) => {
                        if !(4000..=55_000).contains(&rust_server.port)
                            || !(4000..=55_000).contains(&ts_server.port)
                        {
                            case.differences.push(assertion_difference(
                                &restart_case,
                                "/assertions/dev-port",
                                format!(
                                    "expected both dev-server ports in 4000-55000; Rust={}, TypeScript={}",
                                    rust_server.port, ts_server.port
                                ),
                            ));
                        }
                        println!(
                            "{} {}",
                            if case.differences.is_empty() {
                                "PASS"
                            } else {
                                "DIFF"
                            },
                            case.case
                        );
                        recorder.record(journal, case)?;
                        let probe_case =
                            format!("build-{template_type}-restarted-dev-http-reachable");
                        let mut probe_spec = get_spec("/".to_string());
                        probe_spec.headers.insert("host".into(), "localhost".into());
                        let rust_dev_endpoint = format!(
                            "{}:{}",
                            rust_dev_url.trim_end_matches('/'),
                            rust_server.port
                        );
                        let ts_dev_endpoint =
                            format!("{}:{}", ts_dev_url.trim_end_matches('/'), ts_server.port);
                        let Some((case, _, _)) = run_gated_pair(
                            client,
                            journal,
                            recorder,
                            &probe_case,
                            &rust_dev_endpoint,
                            &ts_dev_endpoint,
                            &probe_spec,
                            &probe_spec,
                            RequestTarget::DevServer,
                        )
                        .await?
                        else {
                            record_blocked_stages(
                                journal,
                                recorder,
                                &stages_after_stop(template_type),
                            )?;
                            stop_dev_cleanup(
                                client,
                                journal,
                                rust_api_url,
                                &format!("build-{template_type}-stop-dev-cleanup"),
                                project_id,
                                "rust",
                                &rust_server,
                            )
                            .await;
                            stop_dev_cleanup(
                                client,
                                journal,
                                ts_api_url,
                                &format!("build-{template_type}-stop-dev-cleanup"),
                                project_id,
                                "typescript",
                                &ts_server,
                            )
                            .await;
                            continue;
                        };
                        println!("{} {}", if case.equal { "PASS" } else { "DIFF" }, case.case);
                        recorder.record(journal, case)?;

                        let stop_case = format!("build-{template_type}-stop-dev");
                        let rust_stop = get_spec(format!(
                            "/api/build/stop-dev?projectId={project_id}&pid={}",
                            rust_server.pid
                        ));
                        let ts_stop = get_spec(format!(
                            "/api/build/stop-dev?projectId={project_id}&pid={}",
                            ts_server.pid
                        ));
                        let Some((mut case, rust_stop_response, ts_stop_response)) =
                            run_gated_pair(
                                client,
                                journal,
                                recorder,
                                &stop_case,
                                rust_api_url,
                                ts_api_url,
                                &rust_stop,
                                &ts_stop,
                                RequestTarget::Api,
                            )
                            .await?
                        else {
                            record_blocked_stages(
                                journal,
                                recorder,
                                &stages_after_stop(template_type),
                            )?;
                            stop_dev_cleanup(
                                client,
                                journal,
                                rust_api_url,
                                &format!("build-{template_type}-stop-dev-cleanup"),
                                project_id,
                                "rust",
                                &rust_server,
                            )
                            .await;
                            stop_dev_cleanup(
                                client,
                                journal,
                                ts_api_url,
                                &format!("build-{template_type}-stop-dev-cleanup"),
                                project_id,
                                "typescript",
                                &ts_server,
                            )
                            .await;
                            continue;
                        };
                        for (side, response) in [
                            ("rust", &rust_stop_response),
                            ("typescript", &ts_stop_response),
                        ] {
                            if !json_bool(&response.body, "success") {
                                case.differences.push(assertion_difference(
                                    &stop_case,
                                    &format!("/assertions/{side}/success"),
                                    "stop-dev response must contain success=true".to_string(),
                                ));
                            }
                        }
                        println!(
                            "{} {}",
                            if case.differences.is_empty() {
                                "PASS"
                            } else {
                                "DIFF"
                            },
                            case.case
                        );
                        recorder.record(journal, case)?;

                        // A cleared management entry does not prove the dev server exited.
                        // Probe each side's actual Vite port after stop so a success response
                        // cannot hide a still-serving process and a stale list cannot create a
                        // false failure.
                        let stop_probe_case =
                            format!("build-{template_type}-stop-dev-port-unreachable");
                        let mut stop_probe_spec = get_spec("/".to_string());
                        stop_probe_spec
                            .headers
                            .insert("host".into(), "localhost".into());
                        let rust_dev_endpoint = format!(
                            "{}:{}",
                            rust_dev_url.trim_end_matches('/'),
                            rust_server.port
                        );
                        let ts_dev_endpoint =
                            format!("{}:{}", ts_dev_url.trim_end_matches('/'), ts_server.port);
                        let ts_probe = exchange(
                            client,
                            journal,
                            &stop_probe_case,
                            "typescript",
                            &ts_dev_endpoint,
                            &stop_probe_spec,
                            RequestTarget::DevServer,
                        )
                        .await?;
                        let rust_probe = exchange(
                            client,
                            journal,
                            &stop_probe_case,
                            "rust",
                            &rust_dev_endpoint,
                            &stop_probe_spec,
                            RequestTarget::DevServer,
                        )
                        .await?;
                        let mut stop_probe_differences = Vec::new();
                        for (side, endpoint, probe) in [
                            ("rust", &rust_dev_endpoint, &rust_probe),
                            ("typescript", &ts_dev_endpoint, &ts_probe),
                        ] {
                            let accepts_connections = if probe.status.is_some() {
                                Ok(true)
                            } else {
                                dev_port_accepts_connections(endpoint).await
                            };
                            match accepts_connections {
                                Ok(false) => {}
                                Ok(true) => stop_probe_differences.push(assertion_difference(
                                    &stop_probe_case,
                                    &format!("/assertions/{side}/port-stopped"),
                                    "dev server port still accepts connections after stop"
                                        .to_string(),
                                )),
                                Err(error) => stop_probe_differences.push(assertion_difference(
                                    &stop_probe_case,
                                    &format!("/assertions/{side}/port-stop-unconfirmed"),
                                    format!("could not confirm stopped dev-server port: {error}"),
                                )),
                            }
                        }
                        let stop_probe_result = CaseResult {
                            case: stop_probe_case.clone(),
                            equal: stop_probe_differences.is_empty(),
                            compared:
                                "stopped dev-server port must refuse TCP after HTTP is unreachable"
                                    .into(),
                            normalized_paths: Vec::new(),
                            normalized_headers: Vec::new(),
                            rust_status: rust_probe.status.map(|status| status.as_u16()),
                            ts_status: ts_probe.status.map(|status| status.as_u16()),
                            differences: stop_probe_differences,
                            blocked_by: None,
                        };
                        println!(
                            "{} {}",
                            if stop_probe_result.differences.is_empty() {
                                "PASS"
                            } else {
                                "DIFF"
                            },
                            stop_probe_result.case
                        );
                        recorder.record(journal, stop_probe_result)?;

                        let list_case = format!("build-{template_type}-list-after-stop");
                        let list_spec = get_spec("/api/build/list-dev".to_string());
                        let Some((mut case, rust_list, ts_list)) = run_gated_pair(
                            client,
                            journal,
                            recorder,
                            &list_case,
                            rust_api_url,
                            ts_api_url,
                            &list_spec,
                            &list_spec,
                            RequestTarget::Api,
                        )
                        .await?
                        else {
                            continue;
                        };
                        for (side, response) in [("rust", &rust_list), ("typescript", &ts_list)] {
                            if response_has_project(&response.body, project_id) {
                                case.differences.push(assertion_difference(
                                    &list_case,
                                    &format!("/assertions/{side}/project-stopped"),
                                    format!("project {project_id} remains in list-dev after stop"),
                                ));
                            }
                        }
                        println!(
                            "{} {}",
                            if case.differences.is_empty() {
                                "PASS"
                            } else {
                                "DIFF"
                            },
                            case.case
                        );
                        recorder.record(journal, case)?;
                    }
                    (rust, ts) => {
                        if let Err(error) = rust {
                            case.differences.push(assertion_difference(
                                &restart_case,
                                "/assertions/rust/restart-response",
                                error,
                            ));
                        }
                        if let Err(error) = ts {
                            case.differences.push(assertion_difference(
                                &restart_case,
                                "/assertions/typescript/restart-response",
                                error,
                            ));
                        }
                        println!(
                            "{} {}",
                            if case.differences.is_empty() {
                                "PASS"
                            } else {
                                "DIFF"
                            },
                            case.case
                        );
                        recorder.record(journal, case)?;
                        record_blocked_stages(
                            journal,
                            recorder,
                            &stages_after_stop(template_type),
                        )?;
                        // The restart killed any previously running dev servers on the
                        // failing side; stop a server that this restart did report before
                        // the failure so it cannot leak past the scenario.
                        if let Ok(server) = parse_dev_server(&rust_restart, "rust", project_id) {
                            stop_dev_cleanup(
                                client,
                                journal,
                                rust_api_url,
                                &format!("build-{template_type}-stop-dev-cleanup"),
                                project_id,
                                "rust",
                                &server,
                            )
                            .await;
                        }
                        if let Ok(server) = parse_dev_server(&ts_restart, "typescript", project_id)
                        {
                            stop_dev_cleanup(
                                client,
                                journal,
                                ts_api_url,
                                &format!("build-{template_type}-stop-dev-cleanup"),
                                project_id,
                                "typescript",
                                &server,
                            )
                            .await;
                        }
                    }
                }
            }
            (rust, ts) => {
                if let Err(error) = rust {
                    case.differences.push(assertion_difference(
                        &start_case,
                        "/assertions/rust/start-response",
                        error,
                    ));
                }
                if let Err(error) = ts {
                    case.differences.push(assertion_difference(
                        &start_case,
                        "/assertions/typescript/start-response",
                        error,
                    ));
                }
                println!(
                    "{} {}",
                    if case.differences.is_empty() {
                        "PASS"
                    } else {
                        "DIFF"
                    },
                    case.case
                );
                recorder.record(journal, case)?;
                record_blocked_stages(journal, recorder, &stages_after_start(template_type))?;
                // A start response that cannot be parsed still may have spawned a dev
                // server. If the body carried a usable identity, stop it explicitly.
                if let Ok(server) = parse_dev_server(&rust_start, "rust", project_id) {
                    stop_dev_cleanup(
                        client,
                        journal,
                        rust_api_url,
                        &format!("build-{template_type}-stop-dev-cleanup"),
                        project_id,
                        "rust",
                        &server,
                    )
                    .await;
                }
                if let Ok(server) = parse_dev_server(&ts_start, "typescript", project_id) {
                    stop_dev_cleanup(
                        client,
                        journal,
                        ts_api_url,
                        &format!("build-{template_type}-stop-dev-cleanup"),
                        project_id,
                        "typescript",
                        &server,
                    )
                    .await;
                }
            }
        }
    }
    Ok(())
}

fn parse_dev_server(
    exchange: &Exchange,
    side: &str,
    project_id: &str,
) -> Result<DevServerIdentity, String> {
    if !exchange.status.is_some_and(|status| status.is_success()) {
        return Err(format!(
            "{side} start response was HTTP {:?}",
            exchange.status
        ));
    }
    let body: Value = serde_json::from_slice(&exchange.body)
        .map_err(|error| format!("{side} start response is not JSON: {error}"))?;
    if body.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(format!("{side} start response did not set success=true"));
    }
    if body.get("projectId").and_then(Value::as_str) != Some(project_id) {
        return Err(format!(
            "{side} start response projectId does not match {project_id}"
        ));
    }
    let pid = body
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|pid| *pid > 0)
        .ok_or_else(|| format!("{side} start response pid must be a positive integer"))?;
    let port = body
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port > 0)
        .ok_or_else(|| format!("{side} start response port must be a valid TCP port"))?;
    Ok(DevServerIdentity { pid, port })
}

fn json_bool(body: &[u8], field: &str) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get(field).and_then(Value::as_bool))
        == Some(true)
}

fn validate_git_commit_response(body: &[u8]) -> Result<String, String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("commit response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("commit response must contain success=true".into());
    }
    if value.get("message").and_then(Value::as_str) != Some("Commit successful") {
        return Err("initial non-empty commit must return 'Commit successful'".into());
    }
    let hash = value
        .get("commit")
        .and_then(Value::as_str)
        .ok_or_else(|| "commit response must contain a commit hash".to_string())?;
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "commit must return a full 40-digit SHA-1 hash, got {hash:?}"
        ));
    }
    Ok(hash.to_string())
}

fn validate_git_conflicted_status(body: &[u8], expected_path: &str) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("Git status response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("Git status response must contain success=true".into());
    }
    let conflicted = value
        .get("conflicted")
        .and_then(Value::as_array)
        .ok_or_else(|| "Git status response must contain a conflicted array".to_string())?;
    if !conflicted
        .iter()
        .any(|path| path.as_str() == Some(expected_path))
    {
        return Err(format!(
            "Git status conflicted list must contain {expected_path:?}, got {conflicted:?}"
        ));
    }
    Ok(())
}

fn validate_git_log_response(body: &[u8]) -> Result<String, String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("git log response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("git log response must contain success=true".into());
    }
    if value.get("total").and_then(Value::as_u64) != Some(2) {
        return Err("git log should return the two fixture commits".into());
    }
    let commits = value
        .get("commits")
        .and_then(Value::as_array)
        .filter(|commits| commits.len() == 2)
        .ok_or_else(|| "git log should contain two commit entries".to_string())?;
    let commit = &commits[0];
    let hash = commit
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| "git log entry must contain hash".to_string())?;
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("git log entry must contain a full 40-digit SHA-1 hash".into());
    }
    let date = commit
        .get("date")
        .and_then(Value::as_str)
        .ok_or_else(|| "git log entry must contain date".to_string())?;
    chrono::DateTime::parse_from_rfc3339(date)
        .map_err(|error| format!("git log date must be RFC 3339: {error}"))?;
    for (index, expected_message) in ["A/B second commit", "A/B initial commit"]
        .into_iter()
        .enumerate()
    {
        let entry = &commits[index];
        let entry_hash = entry
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("git log entry {index} must contain hash"))?;
        if entry_hash.len() != 40 || !entry_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "git log entry {index} must contain a full 40-digit SHA-1 hash"
            ));
        }
        let date = entry
            .get("date")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("git log entry {index} must contain date"))?;
        chrono::DateTime::parse_from_rfc3339(date)
            .map_err(|error| format!("git log entry {index} date must be RFC 3339: {error}"))?;
        if entry.get("author_name").and_then(Value::as_str) != Some("File Server A-B")
            || entry.get("author_email").and_then(Value::as_str) != Some("ab@example.invalid")
            || entry
                .get("message")
                .and_then(Value::as_str)
                .is_none_or(|message| message.trim_end_matches('\n') != expected_message)
        {
            return Err(format!(
                "git log entry {index} must contain the fixture author and message {expected_message:?}"
            ));
        }
    }
    Ok(hash.to_string())
}

/// Assert the value looks like a semantic version; implementations declare their
/// own contract line and must not echo each other's number.
fn validate_semver_field(body: &[u8], field: &str) -> Result<(), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("response is not JSON: {error}"))?;
    let version = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("field {field} must be a string"))?;
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || !parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!(
            "field {field} must be a semantic version, got {version:?}"
        ));
    }
    Ok(())
}

fn validate_meta_response(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("file metadata response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("file metadata response must contain success=true".into());
    }
    let metas = value
        .get("metas")
        .and_then(Value::as_array)
        .filter(|metas| metas.len() == 2)
        .ok_or_else(|| "file metadata response must contain two requested entries".to_string())?;
    for (entry, path) in metas.iter().zip(["README.md", "sub/nested/hello.txt"]) {
        if entry.get("path").and_then(Value::as_str) != Some(path)
            || entry.get("isDir").and_then(Value::as_bool) != Some(false)
            || entry.get("isLink").and_then(Value::as_bool) != Some(false)
            || !entry
                .get("size")
                .and_then(Value::as_u64)
                .is_some_and(|size| size > 0)
            || !entry
                .get("mtimeMs")
                .and_then(Value::as_f64)
                .is_some_and(|mtime| mtime > 0.0)
        {
            return Err(format!(
                "file metadata entry for {path} is incomplete or invalid"
            ));
        }
    }
    Ok(())
}

fn validate_boundary_meta_response(body: &[u8], side: &str) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("boundary file metadata response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("boundary file metadata response must contain success=true".into());
    }
    let metas = value
        .get("metas")
        .and_then(Value::as_array)
        .filter(|metas| metas.len() == 3)
        .ok_or_else(|| {
            "boundary metadata response must contain three requested entries".to_string()
        })?;
    for (index, path) in [(0, ".hidden.txt"), (2, "binary.bin")] {
        let entry = &metas[index];
        if entry.get("path").and_then(Value::as_str) != Some(path)
            || entry.get("isDir").and_then(Value::as_bool) != Some(false)
            || entry.get("isLink").and_then(Value::as_bool) != Some(false)
            || !entry
                .get("mtimeMs")
                .and_then(Value::as_f64)
                .is_some_and(|mtime| mtime > 0.0)
        {
            return Err(format!(
                "boundary metadata entry for {path} is incomplete or invalid"
            ));
        }
    }
    // metas[1] 是带首尾空格的文件名；两侧按已批准语义分歧行为不同：
    // Rust 精确寻址返回完整元数据；TS trim 后报 ENOENT（TS 已知缺陷）。
    let spaced = &metas[1];
    if side == "rust" {
        let full = spaced
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| path == "  中文文件 .txt  ");
        let complete = spaced.get("error").is_none()
            && spaced.get("isDir").and_then(Value::as_bool) == Some(false)
            && spaced.get("isLink").and_then(Value::as_bool) == Some(false)
            && spaced.get("size").and_then(Value::as_u64) == Some(24)
            && spaced
                .get("mtimeMs")
                .and_then(Value::as_f64)
                .is_some_and(|mtime| mtime > 0.0);
        if !full || !complete {
            return Err("Rust must resolve the exact spaced file name with full metadata".into());
        }
    } else if spaced.get("path").and_then(Value::as_str) != Some("中文文件 .txt")
        || spaced.get("error").and_then(Value::as_str) != Some("ENOENT")
    {
        return Err(
            "TypeScript is expected to trim the name and report ENOENT (known defect)".into(),
        );
    }
    Ok(())
}

/// Per-side dev-log page contract: success, echoed startIndex, non-empty first page,
/// string content and consecutive line numbers on every entry, totalLines covering
/// the last returned line, and the temp-log file name shape. Log TEXT may differ
/// between implementations; this contract must hold on each side independently.
fn validate_log_page(body: &[u8], requested_start: u64) -> Result<(u64, u64), String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("log page is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("log page must contain success=true".into());
    }
    if value.get("startIndex").and_then(Value::as_u64) != Some(requested_start) {
        return Err(format!(
            "startIndex must echo the requested start {requested_start}"
        ));
    }
    let log_file = value
        .get("logFileName")
        .and_then(Value::as_str)
        .ok_or_else(|| "logFileName must be a string".to_string())?;
    let stamp = log_file
        .strip_prefix("dev-temp-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .filter(|stamp| !stamp.is_empty() && stamp.bytes().all(|byte| byte.is_ascii_digit()));
    if stamp.is_none() {
        return Err(format!(
            "temp log file name must look like dev-temp-<epoch-millis>.log, got {log_file:?}"
        ));
    }
    let logs = value
        .get("logs")
        .and_then(Value::as_array)
        .ok_or_else(|| "logs must be an array".to_string())?;
    if requested_start == 1 && logs.is_empty() {
        return Err("the first log page must not be empty".into());
    }
    let mut last_line = requested_start.saturating_sub(1);
    for entry in logs {
        let line = entry
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| "each log entry must carry a numeric line".to_string())?;
        if entry.get("content").and_then(Value::as_str).is_none() {
            return Err("each log entry must carry a string content".into());
        }
        if line != last_line + 1 {
            return Err(format!(
                "log line numbers must be consecutive from {requested_start}; got {line} after {last_line}"
            ));
        }
        last_line = line;
    }
    let total = value
        .get("totalLines")
        .and_then(Value::as_u64)
        .ok_or_else(|| "totalLines must be a number".to_string())?;
    if total < last_line {
        return Err(format!(
            "totalLines {total} is smaller than the last returned line {last_line}"
        ));
    }
    Ok((last_line, total))
}

fn response_has_project(body: &[u8], project_id: &str) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("list").and_then(Value::as_array).cloned())
        .is_some_and(|list| {
            list.iter()
                .any(|entry| entry.get("projectId").and_then(Value::as_str) == Some(project_id))
        })
}

fn assertion_difference(case: &str, path: &str, message: String) -> Difference {
    diff(case, path, "assertion_failed", Some(json!(message)), None)
}

// Both implementations can require side-specific dynamic values (pid, assigned ports) while
// keeping the HTTP request path and comparison rules explicit.
#[allow(clippy::too_many_arguments)]
async fn run_pair_specs(
    client: &Client,
    journal: &mut RequestJournal,
    case: &str,
    rust_url: &str,
    ts_url: &str,
    rust_spec: &RequestSpec,
    ts_spec: &RequestSpec,
    target: RequestTarget,
) -> Result<(CaseResult, Exchange, Exchange)> {
    let ts = exchange(client, journal, case, "typescript", ts_url, ts_spec, target).await?;
    let rust = exchange(client, journal, case, "rust", rust_url, rust_spec, target).await?;
    let normalized_paths = rust_spec
        .normalized_paths
        .iter()
        .chain(ts_spec.normalized_paths.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let normalized_headers = rust_spec
        .normalized_headers
        .iter()
        .chain(ts_spec.normalized_headers.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let health_probe = rust_spec.health_probe || ts_spec.health_probe;
    // Static file serving carries browser-facing validator/caching semantics that the
    // header protocol layer enforces more strictly than framework defaults on APIs.
    // Those semantics bind representations (2xx/206/304); on error responses Express's
    // framework-default headers are noise, so strictness only applies when BOTH sides
    // returned a successful representation (a status mismatch is reported separately).
    let is_representation = |exchange: &Exchange| {
        exchange.status.is_some_and(|status| {
            status.is_success()
                || status == StatusCode::PARTIAL_CONTENT
                || status == StatusCode::NOT_MODIFIED
        })
    };
    let static_representation = ["/api/page/static/", "/api/computer/static/"]
        .iter()
        .any(|prefix| rust_spec.path.starts_with(prefix))
        || ["/api/page/static/", "/api/computer/static/"]
            .iter()
            .any(|prefix| ts_spec.path.starts_with(prefix));
    let static_representation =
        static_representation && is_representation(&rust) && is_representation(&ts);
    let differences = compare_exchange(
        case,
        &rust,
        &ts,
        health_probe,
        rust_spec.expected_status,
        &normalized_paths,
        &normalized_headers,
        static_representation,
    );
    let equal = differences.is_empty();
    let case_result = CaseResult {
        case: case.to_string(),
        equal,
        compared: if health_probe {
            "expected HTTP class, HTTP status and JSON status field; runtime metadata is not gated"
                .into()
        } else {
            "expected HTTP class, HTTP status, selected stable headers, and complete response body"
                .into()
        },
        normalized_paths,
        normalized_headers,
        rust_status: rust.status.map(|status| status.as_u16()),
        ts_status: ts.status.map(|status| status.as_u16()),
        differences,
        blocked_by: None,
    };
    Ok((case_result, rust, ts))
}

/// Confirm that a dev-server port is actually closed after stop. An HTTP transport error alone
/// could also mean DNS or another network failure, so require the TCP connect to be refused.
async fn dev_port_accepts_connections(endpoint: &str) -> Result<bool, String> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|error| format!("parse dev-server endpoint: {error}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| "dev-server endpoint has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "dev-server endpoint has no port".to_string())?;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("resolve dev-server host: {error}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err("dev-server hostname resolved to no addresses".into());
    }

    for address in addresses {
        match tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(address),
        )
        .await
        {
            Ok(Ok(_)) => return Ok(true),
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Ok(Err(error)) => return Err(format!("connect to {address}: {error}")),
            Err(_) => return Err(format!("connect to {address} timed out")),
        }
    }
    Ok(false)
}

async fn exchange(
    client: &Client,
    journal: &mut RequestJournal,
    case: &str,
    side: &str,
    base_url: &str,
    spec: &RequestSpec,
    target: RequestTarget,
) -> Result<Exchange> {
    let url = endpoint(base_url, &spec.path);
    let mut request = client.request(spec.method.clone(), &url);
    let mut request_headers = BTreeMap::new();
    if let Some(content_type) = spec.content_type {
        request = request.header("content-type", content_type);
        request_headers.insert("content-type".to_string(), content_type.to_string());
    }
    for (name, value) in &spec.headers {
        request = request.header(name, value);
        request_headers.insert(name.clone(), value.clone());
    }
    if !spec.body.is_empty() {
        request = request.body(spec.body.clone());
    }
    let request_body_sha256 = sha256(&spec.body);
    let (request_body_file, request_body_truncated) = if spec.body.is_empty() {
        (None, false)
    } else {
        let (file, truncated) = save_body(&journal.report_dir, case, side, "request", &spec.body)?;
        (Some(file), truncated)
    };
    let request_id = uuid::Uuid::now_v7().to_string();
    journal.begin(&RequestStartLine {
        request_id: request_id.clone(),
        case: case.to_string(),
        side: side.to_string(),
        method: spec.method.to_string(),
        url: url.clone(),
        started_at: timestamp_rfc3339(),
    })?;
    let started = std::time::Instant::now();
    let response = request.timeout(spec.timeout).send().await;
    let headers_elapsed_ms = response
        .as_ref()
        .ok()
        .map(|_| started.elapsed().as_millis());
    let exchange = match response {
        Ok(response) => {
            let status = response.status();
            let headers = response.headers().clone();
            let body = match response.bytes().await {
                Ok(body) => body.to_vec(),
                Err(error) => {
                    let message = format!("read response body: {error:#}");
                    let record = RequestLine {
                        request_id: request_id.clone(),
                        case: case.to_string(),
                        side: side.to_string(),
                        method: spec.method.to_string(),
                        url: url.clone(),
                        request_headers,
                        request_body_bytes: spec.body.len(),
                        request_body_sha256,
                        request_body_file,
                        status: Some(status.as_u16()),
                        response_headers: headers_map(&headers),
                        response_body_bytes: None,
                        response_body_sha256: None,
                        response_body_file: None,
                        request_body_truncated,
                        response_body_truncated: false,
                        headers_elapsed_ms,
                        elapsed_ms: started.elapsed().as_millis(),
                        transport_error: Some(message.clone()),
                    };
                    journal.append(&record, &spec.path, target)?;
                    return Ok(Exchange {
                        status: Some(status),
                        headers,
                        body: Vec::new(),
                        transport_error: Some(message),
                    });
                }
            };
            let (response_body_file, response_body_truncated) =
                save_body(&journal.report_dir, case, side, "response", &body)?;
            let record = RequestLine {
                request_id: request_id.clone(),
                case: case.to_string(),
                side: side.to_string(),
                method: spec.method.to_string(),
                url: url.clone(),
                request_headers,
                request_body_bytes: spec.body.len(),
                request_body_sha256,
                request_body_file,
                status: Some(status.as_u16()),
                response_headers: headers_map(&headers),
                response_body_bytes: Some(body.len()),
                response_body_sha256: Some(sha256(&body)),
                response_body_file: Some(response_body_file),
                request_body_truncated,
                response_body_truncated,
                headers_elapsed_ms,
                elapsed_ms: started.elapsed().as_millis(),
                transport_error: None,
            };
            journal.append(&record, &spec.path, target)?;
            Exchange {
                status: Some(status),
                headers,
                body,
                transport_error: None,
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            let record = RequestLine {
                request_id: request_id.clone(),
                case: case.to_string(),
                side: side.to_string(),
                method: spec.method.to_string(),
                url,
                request_headers,
                request_body_bytes: spec.body.len(),
                request_body_sha256,
                request_body_file,
                status: None,
                response_headers: BTreeMap::new(),
                response_body_bytes: None,
                response_body_sha256: None,
                response_body_file: None,
                request_body_truncated,
                response_body_truncated: false,
                headers_elapsed_ms,
                elapsed_ms: started.elapsed().as_millis(),
                transport_error: Some(message.clone()),
            };
            journal.append(&record, &spec.path, target)?;
            Exchange {
                status: None,
                headers: HeaderMap::new(),
                body: Vec::new(),
                transport_error: Some(message),
            }
        }
    };
    Ok(exchange)
}

/// Outcome of comparing one response header across implementations under its real
/// protocol semantics, validating each side's own value before declaring the two
/// equivalent. Raw values stay in the report either way.
enum HeaderVerdict {
    Equivalent,
    Different,
    Invalid { side: &'static str, reason: String },
}

/// Protocol-level equivalence for headers whose representations legitimately vary
/// across implementations:
/// - `content-type`: identical media type with the charset stated explicitly
///   (`; charset=utf-8`) vs implicitly (JSON's default is UTF-8) is equivalent;
///   different media types or different charsets are not.
/// - `etag`: entity tags are opaque; two well-formed tags are equivalent. On static
///   representations, presence must match (validators are part of that contract).
///   Elsewhere — including error responses — the TypeScript side emits Express
///   framework defaults that the Rust side omits, and the conditional-request
///   contract is enforced by dedicated scenarios.
/// - `content-length`: each side must match its own transferred body; then the two
///   are equivalent by derivation. A mismatch with its own body is a side failure.
/// - `last-modified`: on static content both values are the per-side fixture file
///   mtimes; two well-formed HTTP dates are equivalent there.
///
/// All other headers (and every unlisted case above) compare strictly.
fn header_protocol_verdict(
    header: &str,
    rust_value: Option<&str>,
    ts_value: Option<&str>,
    static_representation: bool,
    rust_body_len: usize,
    ts_body_len: usize,
) -> HeaderVerdict {
    match header {
        "content-type" => match (rust_value, ts_value) {
            (Some(rust), Some(ts)) => {
                match (media_type_and_charset(rust), media_type_and_charset(ts)) {
                    (Some((rust_type, rust_charset)), Some((ts_type, ts_charset))) => {
                        if !rust_type.eq_ignore_ascii_case(&ts_type) {
                            HeaderVerdict::Different
                        } else if rust_charset.eq_ignore_ascii_case(&ts_charset) {
                            HeaderVerdict::Equivalent
                        } else {
                            HeaderVerdict::Different
                        }
                    }
                    _ => HeaderVerdict::Invalid {
                        side: if media_type_and_charset(rust).is_none() {
                            "rust"
                        } else {
                            "typescript"
                        },
                        reason: "content-type must be a media type with optional parameters"
                            .to_string(),
                    },
                }
            }
            _ => HeaderVerdict::Different,
        },
        "etag" => match (rust_value, ts_value) {
            (Some(rust), Some(ts)) => match (entity_tag_shape(rust), entity_tag_shape(ts)) {
                (true, true) => HeaderVerdict::Equivalent,
                (false, _) | (_, false) => HeaderVerdict::Invalid {
                    side: if !entity_tag_shape(rust) {
                        "rust"
                    } else {
                        "typescript"
                    },
                    reason: "etag must be a well-formed entity tag (W/\"…\" or \"…\")".to_string(),
                },
            },
            (None, None) => HeaderVerdict::Equivalent,
            // Static representations serve validators on both sides; a presence
            // mismatch there is material. Elsewhere Express adds weak etags by default.
            _ if static_representation => HeaderVerdict::Different,
            _ => HeaderVerdict::Equivalent,
        },
        "content-length" => {
            let rust_len = rust_value.and_then(|value| value.parse::<usize>().ok());
            let ts_len = ts_value.and_then(|value| value.parse::<usize>().ok());
            match (rust_value.is_some(), ts_value.is_some(), rust_len, ts_len) {
                (false, false, _, _) => HeaderVerdict::Equivalent,
                (true, true, Some(rust_len), Some(ts_len)) => {
                    if rust_len != rust_body_len {
                        HeaderVerdict::Invalid {
                            side: "rust",
                            reason: format!(
                                "content-length {rust_len} does not match the {rust_body_len}-byte body"
                            ),
                        }
                    } else if ts_len != ts_body_len {
                        HeaderVerdict::Invalid {
                            side: "typescript",
                            reason: format!(
                                "content-length {ts_len} does not match the {ts_body_len}-byte body"
                            ),
                        }
                    } else {
                        HeaderVerdict::Equivalent
                    }
                }
                (true, true, _, _) => HeaderVerdict::Invalid {
                    side: if rust_len.is_none() {
                        "rust"
                    } else {
                        "typescript"
                    },
                    reason: "content-length must be a non-negative integer".to_string(),
                },
                _ => HeaderVerdict::Different,
            }
        }
        "last-modified" => match (rust_value, ts_value) {
            (Some(rust), Some(ts)) if static_representation => {
                match (http_date_shape(rust), http_date_shape(ts)) {
                    (true, true) => HeaderVerdict::Equivalent,
                    (false, _) | (_, false) => HeaderVerdict::Invalid {
                        side: if !http_date_shape(rust) {
                            "rust"
                        } else {
                            "typescript"
                        },
                        reason: "last-modified must be an HTTP-date".to_string(),
                    },
                }
            }
            _ => HeaderVerdict::Different,
        },
        _ => HeaderVerdict::Different,
    }
}

/// Split `type/subtype; key=value` into the lowercase media type and an effective
/// charset (`utf-8` when absent, matching JSON's and the implementations' default).
fn media_type_and_charset(value: &str) -> Option<(String, String)> {
    let (media_type, parameters) = match value.split_once(';') {
        Some((media_type, parameters)) => (media_type, parameters),
        None => (value, ""),
    };
    let media_type = media_type.trim();
    let (major, minor) = media_type.split_once('/')?;
    if major.is_empty() || minor.is_empty() || minor.contains('/') {
        return None;
    }
    let charset = parameters
        .split(';')
        .filter_map(|parameter| parameter.trim().split_once('='))
        .find(|(key, _)| key.eq_ignore_ascii_case("charset"))
        .map(|(_, value)| value.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "utf-8".to_string());
    Some((media_type.to_ascii_lowercase(), charset))
}

/// `W/"opaque"` or `"opaque"` with non-empty opaque text and no inner quotes.
fn entity_tag_shape(value: &str) -> bool {
    let body = value.strip_prefix("W/").unwrap_or(value);
    let Some(inner) = body
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return false;
    };
    !inner.is_empty() && !inner.contains('"')
}

/// IMF-fixdate shape (`Sun, 06 Nov 1994 08:49:37 GMT`).
fn http_date_shape(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc2822(value).is_ok()
}

#[allow(clippy::too_many_arguments)]
fn compare_exchange(
    case: &str,
    rust: &Exchange,
    ts: &Exchange,
    health_probe: bool,
    expected_status: ExpectedStatus,
    normalized_paths: &[String],
    normalized_headers: &[String],
    static_representation: bool,
) -> Vec<Difference> {
    let mut differences = Vec::new();
    if rust.transport_error.is_some() || ts.transport_error.is_some() {
        differences.push(Difference {
            case: case.to_string(),
            path: "/transport".into(),
            kind: "transport_error".into(),
            rust_value: rust.transport_error.as_ref().map(|error| json!(error)),
            ts_value: ts.transport_error.as_ref().map(|error| json!(error)),
            accepted: false,
            reason: None,
        });
        return differences;
    }
    for (side, exchange) in [("rust", rust), ("typescript", ts)] {
        if !expected_status.matches(exchange.status) {
            differences.push(diff(
                case,
                &format!("/assertions/{side}/http-status"),
                "assertion_failed",
                Some(json!({
                    "expected": expected_status.label(),
                    "actual": exchange.status.map(|status| status.as_u16()),
                })),
                None,
            ));
        }
    }
    if rust.status != ts.status {
        differences.push(diff(
            case,
            "/http/status",
            "value",
            rust.status.map(|status| json!(status.as_u16())),
            ts.status.map(|status| json!(status.as_u16())),
        ));
    }
    for header in [
        "content-type",
        "content-length",
        "content-range",
        "accept-ranges",
        "cache-control",
        "location",
        "etag",
        "last-modified",
        "access-control-allow-origin",
        "access-control-allow-credentials",
        "access-control-expose-headers",
        "vary",
    ] {
        if normalized_headers
            .iter()
            .any(|normalized| normalized == header)
        {
            continue;
        }
        let rust_value = rust
            .headers
            .get(header)
            .and_then(|value| value.to_str().ok());
        let ts_value = ts.headers.get(header).and_then(|value| value.to_str().ok());
        if rust_value == ts_value {
            continue;
        }
        match header_protocol_verdict(
            header,
            rust_value,
            ts_value,
            static_representation,
            rust.body.len(),
            ts.body.len(),
        ) {
            HeaderVerdict::Equivalent => {}
            HeaderVerdict::Different => {
                differences.push(diff(
                    case,
                    &format!("/http/headers/{header}"),
                    "value",
                    rust_value.map(|value| json!(value)),
                    ts_value.map(|value| json!(value)),
                ));
            }
            HeaderVerdict::Invalid { side, reason } => {
                differences.push(assertion_difference(
                    case,
                    &format!("/assertions/{side}/headers/{header}"),
                    reason,
                ));
            }
        }
    }

    if rust.status.is_none() || ts.status.is_none() {
        return differences;
    }
    if is_zip_exchange(rust) || is_zip_exchange(ts) {
        let rust_entries = zip_semantic_entries(&rust.body);
        let ts_entries = zip_semantic_entries(&ts.body);
        for (side, parsed) in [("rust", &rust_entries), ("typescript", &ts_entries)] {
            if let Err(error) = parsed {
                differences.push(diff(
                    case,
                    &format!("/assertions/{side}/valid-zip"),
                    "assertion_failed",
                    Some(json!(format!(
                        "response declared as ZIP but could not be parsed: {error:#}"
                    ))),
                    None,
                ));
            }
        }
        if let (Ok(rust_entries), Ok(ts_entries)) = (rust_entries, ts_entries) {
            json_differences(
                case,
                "/zip",
                &rust_entries,
                &ts_entries,
                normalized_paths,
                &mut differences,
            );
        }
        return differences;
    }
    let rust_json = serde_json::from_slice::<Value>(&rust.body).ok();
    let ts_json = serde_json::from_slice::<Value>(&ts.body).ok();
    if health_probe {
        let rust_status = rust_json.as_ref().and_then(|value| value.get("status"));
        let ts_status = ts_json.as_ref().and_then(|value| value.get("status"));
        if rust_status != ts_status || rust_status.is_none() || ts_status.is_none() {
            differences.push(diff(
                case,
                "/body/status",
                "value",
                rust_status
                    .cloned()
                    .or_else(|| Some(json!({"$missing": true}))),
                ts_status
                    .cloned()
                    .or_else(|| Some(json!({"$missing": true}))),
            ));
        }
    } else if let (Some(rust_json), Some(ts_json)) = (rust_json, ts_json) {
        json_differences(
            case,
            "",
            &rust_json,
            &ts_json,
            normalized_paths,
            &mut differences,
        );
    } else if rust.body != ts.body {
        differences.push(diff(
            case,
            "/body",
            "bytes",
            Some(json!({"bytes":rust.body.len(),"sha256":sha256(&rust.body)})),
            Some(json!({"bytes":ts.body.len(),"sha256":sha256(&ts.body)})),
        ));
    }
    differences
}

fn is_zip_exchange(exchange: &Exchange) -> bool {
    exchange
        .headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("zip"))
        || exchange.body.starts_with(b"PK\x03\x04")
        || exchange.body.starts_with(b"PK\x05\x06")
}

fn zip_semantic_entries(bytes: &[u8]) -> Result<Value> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("open ZIP archive")?;
    if archive.len() > ZIP_ENTRY_COUNT_LIMIT {
        bail!(
            "ZIP contains {} entries, above the comparison limit {ZIP_ENTRY_COUNT_LIMIT}",
            archive.len()
        );
    }
    let mut total_size = 0_u64;
    let mut entries = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("read ZIP entry {index}"))?;
        let name = entry.name().replace('\\', "/");
        let declared_size = entry.size();
        if declared_size > ZIP_ENTRY_SIZE_LIMIT {
            bail!("ZIP entry {name:?} exceeds the per-entry comparison limit");
        }
        total_size = total_size
            .checked_add(declared_size)
            .context("sum ZIP uncompressed sizes")?;
        if total_size > ZIP_TOTAL_SIZE_LIMIT {
            bail!("ZIP exceeds the total uncompressed comparison limit");
        }
        let is_dir = entry.is_dir();
        let mode = entry.unix_mode();
        let mut content = Vec::with_capacity(declared_size as usize);
        (&mut entry)
            .take(ZIP_ENTRY_SIZE_LIMIT + 1)
            .read_to_end(&mut content)
            .with_context(|| format!("decompress ZIP entry {name:?}"))?;
        if content.len() as u64 > ZIP_ENTRY_SIZE_LIMIT {
            bail!("ZIP entry {name:?} exceeds the per-entry comparison limit");
        }
        if content.len() as u64 != declared_size {
            bail!("ZIP entry {name:?} size did not match its directory record");
        }
        let is_symlink = mode.is_some_and(|mode| mode & 0o170000 == 0o120000);
        let kind = if is_dir {
            "directory"
        } else if is_symlink {
            "symlink"
        } else {
            "file"
        };
        let content_sha = if name == ".npmrc" {
            normalize_generated_npmrc(&content).unwrap_or_else(|| sha256(&content))
        } else {
            sha256(&content)
        };
        let semantic = json!({
            "kind": kind,
            "sha256": if kind == "directory" { None } else { Some(content_sha) },
            "size_bytes": if kind == "directory" { None } else { Some(content.len()) },
            "mode": mode.map(|mode| format!("{:04o}", mode & 0o7777)),
        });
        if entries.insert(name.clone(), semantic).is_some() {
            bail!("ZIP contains duplicate entry path {name:?}");
        }
    }
    // A nonempty directory can be materialized from its child paths during ZIP extraction, so an
    // explicit conventional 0755 directory record is representation-only. Keep empty directories
    // and non-default permission records: those can change the extracted workspace.
    let implied_directories = entries
        .keys()
        .flat_map(|path| {
            let normalized = path.trim_end_matches('/');
            if normalized.is_empty() {
                return Vec::new();
            }
            let components: Vec<_> = normalized.split('/').collect();
            let mut parents = vec![String::new()];
            let mut parent = String::new();
            for component in components.iter().take(components.len().saturating_sub(1)) {
                if !parent.is_empty() {
                    parent.push('/');
                }
                parent.push_str(component);
                parents.push(format!("{parent}/"));
            }
            parents
        })
        .collect::<BTreeSet<_>>();
    for path in implied_directories {
        let is_conventional_nonempty_directory = entries.get(&path).is_some_and(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("directory")
                && entry.get("mode").and_then(Value::as_str) == Some("0755")
        });
        if is_conventional_nonempty_directory {
            entries.remove(&path);
        }
    }
    serde_json::to_value(entries).context("serialize ZIP semantic entries")
}

fn validate_normalized_shape(
    case: &str,
    path: &str,
    rust: Option<&Value>,
    ts: Option<&Value>,
    output: &mut Vec<Difference>,
) {
    let same_shape = match (rust, ts) {
        (Some(a), Some(b)) => std::mem::discriminant(a) == std::mem::discriminant(b),
        _ => false,
    };
    if !same_shape {
        output.push(diff(
            case,
            path,
            "assertion_failed",
            rust.cloned(),
            ts.cloned(),
        ));
    }
}

fn json_differences(
    case: &str,
    path: &str,
    rust: &Value,
    ts: &Value,
    normalized_paths: &[String],
    output: &mut Vec<Difference>,
) {
    match (rust, ts) {
        (Value::Object(rust), Value::Object(ts)) => {
            let keys: BTreeSet<_> = rust.keys().chain(ts.keys()).collect();
            for key in keys {
                let child = format!("{path}/{}", json_pointer_escape(key));
                if normalized_paths
                    .iter()
                    .any(|normalized| normalized == &child)
                {
                    validate_normalized_shape(case, &child, rust.get(key), ts.get(key), output);
                    continue;
                }
                match (rust.get(key), ts.get(key)) {
                    (Some(rust), Some(ts)) => {
                        json_differences(case, &child, rust, ts, normalized_paths, output)
                    }
                    (Some(rust), None) => {
                        output.push(diff(case, &child, "rust_only", Some(rust.clone()), None))
                    }
                    (None, Some(ts)) => {
                        output.push(diff(case, &child, "ts_only", None, Some(ts.clone())))
                    }
                    (None, None) => {}
                }
            }
        }
        (Value::Array(rust), Value::Array(ts)) => {
            if let (Some(rust_by_name), Some(ts_by_name)) =
                (named_array_items(rust), named_array_items(ts))
            {
                let common_names = rust_by_name
                    .keys()
                    .filter(|name| ts_by_name.contains_key(**name))
                    .copied()
                    .collect::<BTreeSet<_>>();
                let rust_order = rust
                    .iter()
                    .filter_map(named_array_item_name)
                    .filter(|name| common_names.contains(name))
                    .collect::<Vec<_>>();
                let ts_order = ts
                    .iter()
                    .filter_map(named_array_item_name)
                    .filter(|name| common_names.contains(name))
                    .collect::<Vec<_>>();
                if rust_order != ts_order {
                    output.push(diff(
                        case,
                        &format!("{path}/$order"),
                        "array_order",
                        Some(json!(rust_order)),
                        Some(json!(ts_order)),
                    ));
                }

                let names = rust_by_name
                    .keys()
                    .chain(ts_by_name.keys())
                    .copied()
                    .collect::<BTreeSet<_>>();
                for name in names {
                    let child = format!("{path}/{}", json_pointer_escape(name));
                    match (rust_by_name.get(name), ts_by_name.get(name)) {
                        (Some(rust), Some(ts)) => {
                            json_differences(case, &child, rust, ts, normalized_paths, output)
                        }
                        (Some(rust), None) => output.push(diff(
                            case,
                            &child,
                            "rust_only",
                            Some((*rust).clone()),
                            None,
                        )),
                        (None, Some(ts)) => {
                            output.push(diff(case, &child, "ts_only", None, Some((*ts).clone())))
                        }
                        (None, None) => {}
                    }
                }
                return;
            }

            let max = rust.len().max(ts.len());
            for index in 0..max {
                let child = format!("{path}/{index}");
                if normalized_paths
                    .iter()
                    .any(|normalized| normalized == &child)
                {
                    validate_normalized_shape(case, &child, rust.get(index), ts.get(index), output);
                    continue;
                }
                match (rust.get(index), ts.get(index)) {
                    (Some(rust), Some(ts)) => {
                        json_differences(case, &child, rust, ts, normalized_paths, output)
                    }
                    (Some(rust), None) => {
                        output.push(diff(case, &child, "rust_only", Some(rust.clone()), None))
                    }
                    (None, Some(ts)) => {
                        output.push(diff(case, &child, "ts_only", None, Some(ts.clone())))
                    }
                    (None, None) => {}
                }
            }
        }
        (Value::String(rust), Value::String(ts)) if path == "/diff" => {
            if normalize_git_diff_hunk_headers(rust) != normalize_git_diff_hunk_headers(ts) {
                output.push(diff(
                    case,
                    path,
                    "value",
                    Some(Value::String(rust.clone())),
                    Some(Value::String(ts.clone())),
                ));
            }
        }
        _ if rust != ts => output.push(diff(
            case,
            path,
            "value",
            Some(rust.clone()),
            Some(ts.clone()),
        )),
        _ => {}
    }
}

/// Git-compatible diff producers may omit the line count when it is one, and they
/// disagree on the start line of an empty range at the top of a file (git writes
/// `-0,0`, gix writes `-1,0`; both mean "insertion before line 1"). Compare those
/// formatting conventions semantically while leaving all non-hunk text intact.
/// Git-compatible diff producers may omit the line count when it is one; that
/// omission is the only universally equivalent formatting difference. The observed
/// gix/system-Git divergence for empty top ranges (`-1,0` vs `-0,0`) is verified only
/// for whole-new-file hunks, so the 0/1 start equivalence is applied exclusively
/// inside `new file mode` blocks. Zero-line counts are always preserved: an empty
/// range (`-2,0`) is never conflated with an omitted one-line range (`-2`).
fn normalize_git_diff_hunk_headers(diff_text: &str) -> String {
    let mut in_new_file_block = false;
    diff_text
        .split_inclusive('\n')
        .map(|line| {
            let (body, newline) = line
                .strip_suffix('\n')
                .map_or((line, ""), |body| (body, "\n"));
            if body.starts_with("diff --git ") {
                in_new_file_block = false;
            } else if body.starts_with("new file mode") {
                in_new_file_block = true;
            }
            normalize_git_diff_hunk_header(body, in_new_file_block)
                .map(|normalized| format!("{normalized}{newline}"))
                .unwrap_or_else(|| line.to_string())
        })
        .collect()
}

fn normalize_git_diff_hunk_header(line: &str, new_file_block: bool) -> Option<String> {
    let ranges = line.strip_prefix("@@ ")?;
    let (ranges, context) = ranges.split_once(" @@")?;
    let (old, new) = ranges.split_once(' ')?;
    if !old.starts_with('-') || !new.starts_with('+') {
        return None;
    }

    fn omit_unit_count(range: &str, new_file_block: bool) -> Option<String> {
        let (sign, coordinates) = range.split_at(1);
        if !matches!(sign, "-" | "+") {
            return None;
        }
        let (start, count) = coordinates
            .split_once(',')
            .map_or((coordinates, None), |(start, count)| (start, Some(count)));
        if start.is_empty() || !start.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        match count {
            // Omitted count means exactly one line (POSIX unified diff).
            Some("1") => Some(format!("{sign}{start}")),
            // For a whole-new-file hunk the old range is empty at the very top; git
            // numbers that start as 0 and gix as 1. Canonicalize only that verified
            // context, and keep the explicit zero count.
            Some("0") if new_file_block && matches!(start, "0" | "1") => Some(format!("{sign}0,0")),
            // Zero-line counts stay explicit; all other counts pass through.
            Some(count) if !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()) => {
                Some(format!("{sign}{start},{count}"))
            }
            None => Some(range.to_string()),
            _ => None,
        }
    }

    Some(format!(
        "@@ {} {} @@{}",
        omit_unit_count(old, new_file_block)?,
        omit_unit_count(new, new_file_block)?,
        context
    ))
}

fn named_array_items(items: &[Value]) -> Option<BTreeMap<&str, &Value>> {
    let mut keyed = BTreeMap::new();
    for item in items {
        let name = named_array_item_name(item)?;
        if keyed.insert(name, item).is_some() {
            return None;
        }
    }
    Some(keyed)
}

fn named_array_item_name(item: &Value) -> Option<&str> {
    item.as_object()?.get("name")?.as_str()
}

fn compare_snapshots(rust: &[SnapshotEntry], ts: &[SnapshotEntry]) -> Result<Vec<Difference>> {
    let rust_by_path: BTreeMap<_, _> = rust
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let ts_by_path: BTreeMap<_, _> = ts
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let paths: BTreeSet<_> = rust_by_path
        .keys()
        .chain(ts_by_path.keys())
        .copied()
        .collect();
    let mut differences = Vec::new();
    for path in paths {
        let rust_entry = rust_by_path.get(path).copied();
        let ts_entry = ts_by_path.get(path).copied();
        if rust_entry != ts_entry {
            differences.push(diff(
                "workspace-state",
                &format!("/{path}"),
                if rust_entry.is_some() && ts_entry.is_some() {
                    "value"
                } else if rust_entry.is_some() {
                    "rust_only"
                } else {
                    "ts_only"
                },
                rust_entry
                    .map(|entry| {
                        serde_json::to_value(entry).context("serialize Rust snapshot entry")
                    })
                    .transpose()?,
                ts_entry
                    .map(|entry| serde_json::to_value(entry).context("serialize TS snapshot entry"))
                    .transpose()?,
            ));
        }
    }
    Ok(differences)
}

fn snapshot_roots(root: &Path) -> Result<Vec<SnapshotEntry>> {
    let mut entries = Vec::new();
    for name in [
        "project-workspace",
        "computer-workspace",
        "project-zips",
        "project-nginx",
    ] {
        let path = root.join(name);
        if path.exists() {
            snapshot_dir(root, &path, &mut entries)?;
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn snapshot_git_state(repo_root: &Path) -> Result<GitStateSnapshot> {
    let repository_exists = repo_root.join(".git").exists();
    let mut snapshot = GitStateSnapshot {
        repository_exists,
        head_reference: None,
        head_tree: None,
        refs: BTreeMap::new(),
        index_entries: Vec::new(),
        status_entries: Vec::new(),
        capture_errors: Vec::new(),
    };
    if !repository_exists {
        snapshot
            .capture_errors
            .push("Git worktree has no .git entry".to_string());
        return Ok(snapshot);
    }

    if let Some(bytes) = capture_git_command(
        repo_root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        &mut snapshot.capture_errors,
    ) {
        snapshot.head_reference = String::from_utf8_lossy(&bytes).trim().to_string().into();
    }
    if let Some(bytes) = capture_git_command(
        repo_root,
        &["rev-parse", "--verify", "HEAD^{tree}"],
        &mut snapshot.capture_errors,
    ) {
        snapshot.head_tree = String::from_utf8_lossy(&bytes).trim().to_string().into();
    }
    if let Some(bytes) = capture_git_command(
        repo_root,
        &["for-each-ref", "--format=%(refname)"],
        &mut snapshot.capture_errors,
    ) {
        for reference in String::from_utf8_lossy(&bytes).lines() {
            if reference.is_empty() {
                continue;
            }
            let revision = format!("{reference}^{{tree}}");
            let args = ["rev-parse", "--verify", revision.as_str()];
            if let Some(tree) = capture_git_command(repo_root, &args, &mut snapshot.capture_errors)
            {
                snapshot.refs.insert(
                    reference.to_string(),
                    String::from_utf8_lossy(&tree).trim().to_string(),
                );
            }
        }
    }
    if let Some(bytes) = capture_git_command(
        repo_root,
        &["ls-files", "--stage", "-z"],
        &mut snapshot.capture_errors,
    ) {
        snapshot.index_entries = git_index_records(&bytes);
    }
    if let Some(bytes) = capture_git_command(
        repo_root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        &mut snapshot.capture_errors,
    ) {
        snapshot.status_entries = nul_records(&bytes);
        snapshot.status_entries.sort();
    }
    Ok(snapshot)
}

fn capture_git_command(
    repo_root: &Path,
    args: &[&str],
    errors: &mut Vec<String>,
) -> Option<Vec<u8>> {
    let output = match ProcessCommand::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            errors.push(format!("git {} could not start: {error}", args.join(" ")));
            return None;
        }
    };
    if output.status.success() {
        Some(output.stdout)
    } else {
        errors.push(format!(
            "git {} exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
        None
    }
}

fn nul_records(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect()
}

fn git_index_records(bytes: &[u8]) -> Vec<String> {
    let mut entries = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(
            |record| match record.iter().position(|byte| *byte == b'\t') {
                Some(separator) => format!(
                    "{}\t{}",
                    String::from_utf8_lossy(&record[separator + 1..]),
                    String::from_utf8_lossy(&record[..separator])
                ),
                None => String::from_utf8_lossy(record).into_owned(),
            },
        )
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

/// Matches the `<epoch-millis>\n` shape both implementations write into
/// `.dynamic_add.lock`.
fn is_timestamp_marker(bytes: &[u8]) -> bool {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    !body.is_empty() && body.iter().all(u8::is_ascii_digit)
}

/// Both implementations stamp generated `.npmrc` files with a local-time comment
/// (`# 自动生成于 YYYY-MM-DD HH:MM:SS`). Independent project creation can straddle a
/// second boundary, so normalize that one line before hashing; any other content
/// difference still compares byte-wise.
fn normalize_generated_npmrc(contents: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(contents).ok()?;
    let mut normalized = String::with_capacity(text.len());
    let mut changed = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        if let Some(stamp) = body.strip_prefix("# 自动生成于 ") {
            let digits_or_sep = |(index, byte): (usize, u8)| match index {
                4 | 7 => byte == b'-',
                10 => byte == b' ',
                13 | 16 => byte == b':',
                _ => byte.is_ascii_digit(),
            };
            if stamp.len() == 19 && stamp.bytes().enumerate().all(digits_or_sep) {
                normalized.push_str("# 自动生成于 <timestamp>\n");
                changed = true;
                continue;
            }
        }
        normalized.push_str(line);
    }
    changed.then_some(normalized)
}

fn snapshot_dir(root: &Path, dir: &Path, entries: &mut Vec<SnapshotEntry>) -> Result<()> {
    let mut children = fs::read_dir(dir)
        .with_context(|| format!("read workspace directory {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if [".git", "node_modules", "cache", "logs", "tmp"].contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = fs::symlink_metadata(&path)?;
        let mode = mode_string(&metadata);
        if metadata.file_type().is_symlink() {
            entries.push(SnapshotEntry {
                path: relative,
                kind: "symlink".into(),
                sha256: None,
                size_bytes: None,
                mode,
                symlink_target: Some(fs::read_link(&path)?.to_string_lossy().into_owned()),
            });
        } else if metadata.is_dir() {
            entries.push(SnapshotEntry {
                path: relative,
                kind: "directory".into(),
                sha256: None,
                size_bytes: None,
                mode,
                symlink_target: None,
            });
            snapshot_dir(root, &path, entries)?;
        } else if metadata.is_file() {
            let contents = fs::read(&path)?;
            // Two state-snapshot normalizations, both shape-preserving:
            // - `.dynamic_add.lock` marks dynamically installed skills; both
            //   implementations write "<epoch-millis>\n", so the value differs only by
            //   installation time. Compare its shape, not the timestamp.
            // - ZIP artifacts embed entry mtimes and compression details, so raw bytes
            //   differ between equivalent archives. Apply the same entry-semantics
            //   comparison used for ZIP HTTP responses; unparseable zips still compare
            //   byte-wise.
            // Existence, kind, and mode are always compared.
            let (sha256, size_bytes) =
                if name == ".dynamic_add.lock" && is_timestamp_marker(&contents) {
                    (Some("<dynamic-timestamp>".to_string()), None)
                } else if name == ".npmrc" {
                    match normalize_generated_npmrc(&contents) {
                        Some(normalized) => (
                            Some(sha256(normalized.as_bytes())),
                            Some(contents.len() as u64),
                        ),
                        None => (Some(sha256(&contents)), Some(contents.len() as u64)),
                    }
                } else if path.extension().is_some_and(|extension| extension == "zip") {
                    match zip_semantic_entries(&contents) {
                        Ok(entries) => {
                            let canonical = serde_json::to_string(&entries)
                                .context("serialize ZIP semantic entries")?;
                            (
                                Some(format!("semantic-zip:{}", sha256(canonical.as_bytes()))),
                                None,
                            )
                        }
                        Err(_) => (Some(sha256(&contents)), Some(contents.len() as u64)),
                    }
                } else {
                    (Some(sha256(&contents)), Some(contents.len() as u64))
                };
            entries.push(SnapshotEntry {
                path: relative,
                kind: "file".into(),
                sha256,
                size_bytes,
                mode,
                symlink_target: None,
            });
        }
    }
    Ok(())
}

fn prepare_computer_fixture(root: &Path) -> Result<()> {
    let workspace = root.join("computer-workspace");
    let dir = root
        .join("computer-workspace")
        .join(CASE_USER)
        .join(CASE_CID);
    fs::create_dir_all(dir.join("sub/nested"))?;
    fs::create_dir_all(dir.join("empty"))?;
    fs::write(dir.join("README.md"), "file-server A/B fixture\n")?;
    fs::write(dir.join("sub/nested/hello.txt"), "nested file\n")?;
    fs::write(
        dir.join("  spaced name.txt  "),
        "spaces are part of the file name\n",
    )?;
    fs::write(dir.join(".hidden.txt"), "hidden fixture\n")?;
    fs::write(dir.join(".gitignore"), "node_modules\n")?;
    fs::write(dir.join("  中文文件 .txt  "), "unicode and edge spaces\n")?;
    fs::write(dir.join("binary.bin"), [0, 1, 2, 13, 10, 127, 255])?;
    let log_dir = workspace.join(CASE_USER).join(AB_LOG_CID).join(".logs");
    fs::create_dir_all(&log_dir)?;
    fs::write(
        log_dir.join("ab.log"),
        "first line\n\nsecond line\nthird line\nfourth line\n",
    )?;
    fs::write(
        workspace.join("file-server-ab-outside-secret.txt"),
        "must remain outside the selected session root\n",
    )?;
    let preserved_agents = workspace
        .join(CASE_USER)
        .join(AB_IMPORT_CID)
        .join(".agents");
    fs::create_dir_all(&preserved_agents)?;
    fs::write(preserved_agents.join("keep.txt"), "preserved agent data")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        symlink("sub/nested/hello.txt", dir.join("inside-link.txt"))?;
        symlink(
            "../../file-server-ab-outside-secret.txt",
            dir.join("outside-link.txt"),
        )?;
    }
    Ok(())
}

async fn wait_health(client: &Client, base_url: &str, side: &str) -> Result<()> {
    let until = tokio::time::Instant::now() + HEALTH_TIMEOUT;
    let url = endpoint(base_url, "/health");
    loop {
        let last_error = match client
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                println!("{side} healthy");
                return Ok(());
            }
            Ok(response) => format!("HTTP {}", response.status()),
            Err(error) => error.to_string(),
        };
        if tokio::time::Instant::now() < until {
            tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
        } else {
            bail!("{side} health timed out at {url}; last error: {last_error}");
        }
    }
}

fn hash_fixtures(fixtures: &Path) -> Result<BTreeMap<String, String>> {
    let mut hashes = BTreeMap::new();
    for file in [
        "react-vite-template.zip",
        "vue3-vite-template.zip",
        "skills-fixture.zip",
        "workspace-project.zip",
        "package-project.zip",
    ] {
        let path = fixtures.join(file);
        let bytes = fs::read(&path).with_context(|| format!("read fixture {}", path.display()))?;
        hashes.insert(file.to_string(), sha256(&bytes));
    }
    Ok(hashes)
}

fn save_body(
    report_dir: &Path,
    case: &str,
    side: &str,
    kind: &str,
    bytes: &[u8],
) -> Result<(String, bool)> {
    let safe_case = case
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let path = PathBuf::from("bodies").join(format!("{safe_case}-{side}-{kind}.bin"));
    let full = report_dir.join(&path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    let truncated = bytes.len() > BODY_CAPTURE_LIMIT;
    fs::write(&full, &bytes[..bytes.len().min(BODY_CAPTURE_LIMIT)])?;
    Ok((path.to_string_lossy().into_owned(), truncated))
}

fn write_summary(path: &Path, run_id: &str, diff: &RunDiff) -> Result<()> {
    let mut summary = fs::File::create(path)?;
    writeln!(summary, "# File-server A/B run {run_id}\n")?;
    writeln!(summary, "- Comparisons: {}", diff.summary.comparisons)?;
    writeln!(summary, "- Equal: {}", diff.summary.equal)?;
    writeln!(
        summary,
        "- Expected differences: {}",
        diff.summary.expected_differences
    )?;
    writeln!(
        summary,
        "- Unclassified differences: {}\n",
        diff.summary.unclassified_differences
    )?;
    writeln!(
        summary,
        "- Environment errors: {}",
        diff.summary.environment_errors
    )?;
    writeln!(
        summary,
        "- Environment blocked: {}\n",
        diff.summary.environment_blocked
    )?;
    writeln!(
        summary,
        "- Precondition errors: {}\n",
        diff.summary.precondition_errors
    )?;
    writeln!(
        summary,
        "- Transport failures (cause unclassified): {}\n",
        diff.summary.transport_errors
    )?;
    writeln!(summary, "- Blocked cases: {}", diff.summary.blocked_cases)?;
    if let Some(reason) = &diff.summary.incomplete_reason {
        writeln!(
            summary,
            "- **Incomplete run**: the driver exited before finishing; cases and requests below are partial. {reason}\n"
        )?;
    }
    writeln!(summary, "## HTTP cases\n")?;
    writeln!(
        summary,
        "| Case | Result | Rust HTTP | TS HTTP | Differences |\n|---|---:|---:|---:|---:|"
    )?;
    for case in &diff.cases {
        writeln!(
            summary,
            "| `{}` | {} | {} | {} | {} |",
            case.case,
            if case.blocked_by.is_some() {
                "blocked"
            } else if case.equal {
                "equal"
            } else {
                "different"
            },
            display_status(case.rust_status),
            display_status(case.ts_status),
            case.differences.len()
        )?;
    }
    writeln!(summary, "\n## Difference field summary (reporting only)\n")?;
    writeln!(
        summary,
        "Counts repeated fields by scope, JSON/header path, kind, and classification. This section does not normalize or waive any difference; `diff.json` remains the full source of truth.\n"
    )?;
    writeln!(
        summary,
        "| Scope | Path | Kind | Classification | Occurrences | Cases (up to 5 shown) |\n|---|---|---|---|---:|---|"
    )?;
    for group in difference_groups(diff) {
        let shown_cases = group.cases.iter().take(5).cloned().collect::<Vec<_>>();
        let omitted = group.cases.len().saturating_sub(shown_cases.len());
        let cases = if omitted == 0 {
            shown_cases.join(", ")
        } else {
            format!("{}, … (+{omitted} more)", shown_cases.join(", "))
        };
        writeln!(
            summary,
            "| {} | {} | {} | {} | {} | {} |",
            markdown_table_cell(&group.scope),
            markdown_table_cell(&group.path),
            markdown_table_cell(&group.kind),
            if group.accepted {
                "expected"
            } else {
                "unclassified"
            },
            group.occurrences,
            markdown_table_cell(&cases)
        )?;
    }
    if diff.cases.iter().all(|case| case.differences.is_empty())
        && diff.initial_state_differences.is_empty()
        && diff.state_differences.is_empty()
        && diff.git_state_differences.is_empty()
    {
        writeln!(summary, "| — | — | — | — | 0 | No differences |")?;
    }
    writeln!(summary, "\n## Initial workspace state differences\n")?;
    if diff.initial_state_differences.is_empty() {
        writeln!(summary, "No initial file-tree differences.")?;
    } else {
        for difference in &diff.initial_state_differences {
            writeln!(summary, "- `{}` `{}`", difference.path, difference.kind)?;
        }
    }
    writeln!(summary, "\n## Final workspace state differences\n")?;
    if diff.state_differences.is_empty() {
        writeln!(summary, "No file-tree differences.")?;
    } else {
        for difference in &diff.state_differences {
            writeln!(summary, "- `{}` `{}`", difference.path, difference.kind)?;
        }
    }
    writeln!(summary, "\n## Git ref, index and status differences\n")?;
    if diff.git_state_differences.is_empty() {
        writeln!(summary, "No captured Git-state differences.")?;
    } else {
        for difference in &diff.git_state_differences {
            writeln!(summary, "- `{}` `{}`", difference.path, difference.kind)?;
        }
    }
    writeln!(summary, "\n## Classified outcome\n")?;
    writeln!(
        summary,
        "Every difference is bucketed by cause; `diff.json` remains the source of truth. Strict exit rules are unchanged by this section.\n"
    )?;
    let mut buckets: BTreeMap<&str, (usize, BTreeSet<String>)> = BTreeMap::new();
    let mut bucket = |name: &'static str, case: &str| {
        let entry = buckets.entry(name).or_insert((0, BTreeSet::new()));
        entry.0 += 1;
        entry.1.insert(case.to_string());
    };
    let mut all_differences = diff
        .cases
        .iter()
        .flat_map(|case| {
            case.differences
                .iter()
                .map(move |d| (case.case.as_str(), d))
        })
        .collect::<Vec<_>>();
    all_differences.extend(
        diff.state_differences
            .iter()
            .map(|d| ("workspace-state", d))
            .chain(diff.git_state_differences.iter().map(|d| ("git-state", d)))
            .chain(
                diff.initial_state_differences
                    .iter()
                    .map(|d| ("initial-state", d)),
            ),
    );
    for (case_name, difference) in all_differences {
        if difference.accepted {
            bucket("approved differences (rules)", case_name);
        } else if difference.kind == "blocked" {
            bucket("blocked cases", case_name);
        } else if difference.kind == "transport_error" {
            bucket("transport failures (cause unclassified)", case_name);
        } else if difference.kind == "assertion_failed" && difference.path.contains("/typescript/")
        {
            bucket("TypeScript-side contract failures", case_name);
        } else if difference.kind == "assertion_failed" && difference.path.contains("/rust/") {
            bucket("Rust-side contract failures", case_name);
        } else if difference.kind == "assertion_failed" {
            bucket("unattributed assertion failures", case_name);
        } else {
            bucket("unexplained differences", case_name);
        }
    }
    for (name, (count, cases)) in &buckets {
        let shown = cases.iter().take(6).cloned().collect::<Vec<_>>();
        let omitted = cases.len().saturating_sub(shown.len());
        let case_list = if omitted == 0 {
            shown.join(", ")
        } else {
            format!("{shown:?} (+{omitted} more)")
        };
        writeln!(summary, "- **{name}**: {count} — cases: {case_list}")?;
    }

    writeln!(summary, "\n## Exact comparison normalizations\n")?;
    let mut any_normalizations = false;
    for case in &diff.cases {
        if !case.normalized_paths.is_empty() || !case.normalized_headers.is_empty() {
            any_normalizations = true;
            let mut normalizations = case
                .normalized_paths
                .iter()
                .map(|path| format!("JSON `{path}`"))
                .collect::<Vec<_>>();
            normalizations.extend(
                case.normalized_headers
                    .iter()
                    .map(|header| format!("header `{header}`")),
            );
            writeln!(summary, "- `{}`: {}", case.case, normalizations.join(", "))?;
        }
    }
    if !any_normalizations {
        writeln!(summary, "No JSON fields or response headers normalized.")?;
    }
    Ok(())
}

fn difference_groups(diff: &RunDiff) -> Vec<DifferenceGroup> {
    let mut grouped: BTreeMap<DifferenceGroupKey, DifferenceGroupCounts> = BTreeMap::new();
    let mut add = |scope: &str, case: &str, difference: &Difference| {
        let key = (
            scope.to_string(),
            difference.path.clone(),
            difference.kind.clone(),
            difference.accepted,
        );
        let (occurrences, cases) = grouped.entry(key).or_default();
        *occurrences += 1;
        cases.insert(case.to_string());
    };

    for case in &diff.cases {
        for difference in &case.differences {
            add("HTTP", &case.case, difference);
        }
    }
    for difference in &diff.initial_state_differences {
        add("initial workspace", "initial snapshot", difference);
    }
    for difference in &diff.state_differences {
        add("workspace", "final snapshot", difference);
    }
    for difference in &diff.git_state_differences {
        add("Git state", "Git snapshot", difference);
    }

    let mut groups = grouped
        .into_iter()
        .map(
            |((scope, path, kind, accepted), (occurrences, cases))| DifferenceGroup {
                scope,
                path,
                kind,
                accepted,
                occurrences,
                cases,
            },
        )
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then_with(|| left.scope.cmp(&right.scope))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.accepted.cmp(&right.accepted))
    });
    groups
}

fn markdown_table_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn display_status(status: Option<u16>) -> String {
    status.map_or_else(|| "transport error".into(), |value| value.to_string())
}

fn diff(
    case: &str,
    path: &str,
    kind: &str,
    rust_value: Option<Value>,
    ts_value: Option<Value>,
) -> Difference {
    Difference {
        case: case.to_string(),
        path: if path.is_empty() {
            "/".into()
        } else {
            path.into()
        },
        kind: kind.into(),
        rust_value,
        ts_value,
        accepted: false,
        reason: None,
    }
}

fn append_jsonl(writer: &mut fs::File, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn headers_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|value| {
                (
                    name.as_str().to_string(),
                    redact_header(name.as_str(), value),
                )
            })
        })
        .collect()
}

fn redact_header(name: &str, value: &str) -> String {
    if matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "set-cookie" | "x-proxy-token"
    ) {
        "[REDACTED]".into()
    } else {
        value.to_string()
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn endpoint(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn encode_query(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn json_pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn timestamp_for_id() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string()
}

fn timestamp_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn validate_run_id(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 100
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("run id must contain 1-100 ASCII letters, digits, '-' or '_' only");
    }
    Ok(value.to_string())
}

fn validate_rules(rules: &RulesFile) -> Result<()> {
    if rules.schema_version != 1 {
        bail!(
            "unsupported A/B rules schema version {}",
            rules.schema_version
        );
    }
    let today = chrono::Utc::now().date_naive();
    let mut keys = BTreeSet::new();
    for rule in &rules.rules {
        if rule.case.trim().is_empty()
            || rule.path.trim().is_empty()
            || !rule.path.starts_with('/')
            || rule.path.contains('*')
            || rule.kind.trim().is_empty()
            || rule.reason.trim().is_empty()
            || rule.reviewed_by.trim().is_empty()
        {
            bail!("each A/B rule requires exact case/path/kind, a reason, and reviewed_by");
        }
        if !matches!(
            rule.kind.as_str(),
            "value" | "rust_only" | "ts_only" | "bytes"
        ) {
            bail!(
                "A/B rules may accept only exact semantic value differences, not {}",
                rule.kind
            );
        }
        let expires_on = chrono::NaiveDate::parse_from_str(&rule.expires_on, "%Y-%m-%d")
            .with_context(|| format!("invalid A/B rule expiry date {}", rule.expires_on))?;
        if expires_on < today {
            bail!(
                "A/B difference rule for {} {} expired on {}",
                rule.case,
                rule.path,
                rule.expires_on
            );
        }
        let key = (&rule.case, &rule.path, &rule.kind);
        if !keys.insert(key) {
            bail!(
                "duplicate A/B difference rule for {} {} {}",
                rule.case,
                rule.path,
                rule.kind
            );
        }
    }
    Ok(())
}

fn apply_rules(differences: &mut [Difference], rules: &RulesFile) {
    for difference in differences {
        let Some(rule) = rules.rules.iter().find(|rule| {
            rule.case == difference.case
                && rule.path == difference.path
                && rule.kind == difference.kind
                && expected_value_matches(&difference.rust_value, &rule.expected_rust)
                && expected_value_matches(&difference.ts_value, &rule.expected_typescript)
        }) else {
            continue;
        };
        difference.accepted = true;
        difference.reason = Some(format!(
            "{} (reviewed by {}, expires {})",
            rule.reason, rule.reviewed_by, rule.expires_on
        ));
    }
}

fn expected_value_matches(actual: &Option<Value>, expected: &Value) -> bool {
    if expected == &json!({"$missing": true}) {
        actual.is_none()
    } else {
        actual.as_ref() == Some(expected)
    }
}

#[cfg(unix)]
fn mode_string(metadata: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    Some(format!("{:04o}", metadata.permissions().mode() & 0o7777))
}

#[cfg(not(unix))]
fn mode_string(_metadata: &fs::Metadata) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_does_not_hide_missing_or_wrong_type() {
        for ts in [json!({}), json!({"port": "1234"})] {
            let mut differences = vec![];
            json_differences(
                "port",
                "",
                &json!({"port": 4321}),
                &ts,
                &["/port".into()],
                &mut differences,
            );
            assert_eq!(differences.len(), 1);
            assert_eq!(differences[0].kind, "assertion_failed");
        }
    }

    fn api_evidence(side: &str, case: &str, method: &str, path: &str) -> RequestEvidence {
        RequestEvidence {
            case: case.to_string(),
            side: side.to_string(),
            method: method.to_string(),
            path: path.to_string(),
            target: RequestTarget::Api,
        }
    }

    fn executed_case(case: &str, differences: Vec<Difference>) -> CaseResult {
        CaseResult {
            case: case.to_string(),
            equal: differences.is_empty(),
            compared: "http".into(),
            normalized_paths: vec![],
            normalized_headers: vec![],
            rust_status: Some(200),
            ts_status: Some(200),
            differences,
            blocked_by: None,
        }
    }

    #[test]
    fn planned_route_is_not_executed_without_a_completed_case() {
        let mut coverage =
            json!({"entries": [{"method": "GET", "path": "/api/x", "cases": ["create"]}]});
        update_route_execution(&mut coverage, &[], &[], Suite::Core).unwrap();
        assert_eq!(coverage["entries"][0]["executed_in_this_run"], false);
        assert_eq!(coverage["entries"][0]["execution_status"], "not_run");
        let case = executed_case(
            "create",
            vec![diff("create", "/transport", "transport_error", None, None)],
        );
        let evidence = vec![
            api_evidence("rust", "create", "GET", "/api/x"),
            api_evidence("typescript", "create", "GET", "/api/x"),
        ];
        update_route_execution(&mut coverage, &[case], &evidence, Suite::Core).unwrap();
        assert_eq!(coverage["entries"][0]["execution_status"], "failed");
    }

    /// A case name listed for a route must not claim execution unless BOTH sides
    /// recorded a matching API request for that case, method, and route template.
    #[test]
    fn route_coverage_requires_matching_request_evidence() {
        let mut coverage = json!({"entries": [{"method": "POST", "path": "/api/computer/files-update", "cases": ["write"]}]});
        let cases = vec![executed_case("write", vec![])];

        // Branch 1: only the Rust side issued a matching request.
        let evidence = vec![api_evidence(
            "rust",
            "write",
            "POST",
            "/api/computer/files-update",
        )];
        let inconsistencies =
            update_route_execution(&mut coverage, &cases, &evidence, Suite::Core).unwrap();
        assert_eq!(coverage["entries"][0]["executed_in_this_run"], false);
        assert_eq!(coverage["entries"][0]["execution_status"], "unverified");
        assert_eq!(inconsistencies.len(), 1);
        assert!(inconsistencies[0].contains("missing typescript"));

        // Branch 2: both sides issued requests, but TypeScript's went to another route
        // (and a dev-server probe never verifies API route coverage).
        let evidence = vec![
            api_evidence("rust", "write", "POST", "/api/computer/files-update"),
            api_evidence("typescript", "write", "POST", "/api/computer/generate-file"),
            RequestEvidence {
                case: "write".into(),
                side: "typescript".into(),
                method: "POST".into(),
                path: "/api/computer/files-update".into(),
                target: RequestTarget::DevServer,
            },
        ];
        let inconsistencies =
            update_route_execution(&mut coverage, &cases, &evidence, Suite::Core).unwrap();
        assert_eq!(coverage["entries"][0]["execution_status"], "unverified");
        assert!(inconsistencies[0].contains("missing typescript"));

        // Branch 3: both sides match; the entry counts as executed and completed.
        // Evidence paths are query-stripped, matching what the journal records.
        let evidence = vec![
            api_evidence("rust", "write", "POST", "/api/computer/files-update"),
            api_evidence("typescript", "write", "POST", "/api/computer/files-update"),
        ];
        let inconsistencies =
            update_route_execution(&mut coverage, &cases, &evidence, Suite::Core).unwrap();
        assert!(inconsistencies.is_empty());
        assert_eq!(coverage["entries"][0]["executed_in_this_run"], true);
        assert_eq!(coverage["entries"][0]["execution_status"], "completed");
    }

    #[test]
    fn route_templates_match_params_wildcards_and_root() {
        assert!(route_template_matches(
            "/api/computer/static/:userId/:cId/*",
            "/api/computer/static/user/session/ab-write/renamed.txt"
        ));
        assert!(route_template_matches(
            "/api/page/static/:projectId/*",
            "/api/page/static/react/src/a.txt"
        ));
        assert!(!route_template_matches(
            "/api/page/static/:projectId/*",
            "/api/page/static"
        ));
        assert!(!route_template_matches(
            "/api/computer/static/:userId/:cId/*",
            "/api/computer/other/user/session/x"
        ));
        assert!(!route_template_matches(
            "/api/computer/static/:userId/:cId",
            "/api/computer/static/user/session/x"
        ));
        assert!(route_template_matches("/", "/"));
        assert!(!route_template_matches("/", "/api/version"));
    }

    /// A failed dependency blocks its dependents instead of letting them run and
    /// produce cascade diffs; the blocked route is reported as blocked, not executed.
    #[test]
    fn failed_dependency_blocks_dependent_without_claiming_execution() {
        let failure = || {
            assertion_difference(
                "computer-upload-file-binary",
                "/assertions/rust/http-status",
                "expected 2xx".into(),
            )
        };
        let upload = executed_case("computer-upload-file-binary", vec![failure()]);
        assert!(case_failed(&upload));
        let dependencies = case_dependencies("computer-read-uploaded-single-binary");
        assert_eq!(dependencies, ["computer-upload-file-binary"]);
        let blocker = find_blocker(&[upload], dependencies).expect("upload failure blocks read");
        assert_eq!(blocker, "computer-upload-file-binary");

        let blocked = blocked_case("computer-read-uploaded-single-binary", &blocker);
        assert_eq!(
            blocked.blocked_by.as_deref(),
            Some("computer-upload-file-binary")
        );
        assert!(!blocked.equal);
        // A cosmetic-only difference must not block dependents.
        let cosmetic = executed_case(
            "computer-upload-file-binary",
            vec![diff(
                "computer-upload-file-binary",
                "/http/headers/etag",
                "value",
                Some(json!("a")),
                Some(json!("b")),
            )],
        );
        assert!(!case_failed(&cosmetic));
        assert!(find_blocker(&[cosmetic], dependencies).is_none());

        // Route coverage distinguishes blocked from executed routes.
        let mut coverage = json!({
            "entries": [
                {"method": "GET", "path": "/api/computer/static/:userId/:cId/*", "cases": ["computer-read-uploaded-single-binary"]},
                {"method": "POST", "path": "/api/computer/upload-file", "cases": ["computer-upload-file-binary"]}
            ]
        });
        let evidence = vec![
            api_evidence(
                "rust",
                "computer-upload-file-binary",
                "POST",
                "/api/computer/upload-file",
            ),
            api_evidence(
                "typescript",
                "computer-upload-file-binary",
                "POST",
                "/api/computer/upload-file",
            ),
        ];
        let cases = vec![
            executed_case("computer-upload-file-binary", vec![failure()]),
            blocked,
        ];
        let inconsistencies =
            update_route_execution(&mut coverage, &cases, &evidence, Suite::Core).unwrap();
        assert!(inconsistencies.is_empty());
        assert_eq!(coverage["entries"][0]["execution_status"], "blocked");
        assert_eq!(
            coverage["entries"][0]["blocked_cases"],
            json!(["computer-read-uploaded-single-binary"])
        );
        assert_eq!(coverage["entries"][1]["execution_status"], "failed");
    }

    #[test]
    fn difference_field_summary_groups_repeats_without_accepting_them() {
        let repeated_a = diff(
            "header-a",
            "/http/headers/content-type",
            "value",
            Some(json!("application/json")),
            Some(json!("application/json; charset=utf-8")),
        );
        let repeated_b = diff(
            "header-b",
            "/http/headers/content-type",
            "value",
            Some(json!("text/html")),
            Some(json!("text/html; charset=utf-8")),
        );
        let state_difference = diff(
            "state",
            "/http/headers/content-type",
            "value",
            Some(json!("application/json")),
            Some(json!("application/json; charset=utf-8")),
        );
        let report = RunDiff {
            cases: vec![
                CaseResult {
                    case: "header-a".into(),
                    equal: false,
                    compared: "http".into(),
                    normalized_paths: Vec::new(),
                    normalized_headers: Vec::new(),
                    rust_status: Some(200),
                    ts_status: Some(200),
                    differences: vec![repeated_a],
                    blocked_by: None,
                },
                CaseResult {
                    case: "header-b".into(),
                    equal: false,
                    compared: "http".into(),
                    normalized_paths: Vec::new(),
                    normalized_headers: Vec::new(),
                    rust_status: Some(200),
                    ts_status: Some(200),
                    differences: vec![repeated_b],
                    blocked_by: None,
                },
            ],
            initial_state_differences: Vec::new(),
            state_differences: vec![state_difference],
            git_state_differences: Vec::new(),
            summary: Summary {
                comparisons: 3,
                equal: 0,
                expected_differences: 0,
                unclassified_differences: 3,
                transport_errors: 0,
                environment_errors: 0,
                environment_blocked: false,
                precondition_errors: 0,
                blocked_cases: 0,
                incomplete_reason: None,
            },
        };

        let groups = difference_groups(&report);
        let http_header = groups
            .iter()
            .find(|group| group.scope == "HTTP")
            .expect("HTTP header group");
        assert_eq!(http_header.path, "/http/headers/content-type");
        assert_eq!(http_header.occurrences, 2);
        assert_eq!(http_header.cases.len(), 2);
        assert!(!http_header.accepted);

        let workspace_header = groups
            .iter()
            .find(|group| group.scope == "workspace")
            .expect("workspace state group");
        assert_eq!(workspace_header.occurrences, 1);
        assert_eq!(workspace_header.cases.len(), 1);
        assert!(!workspace_header.accepted);

        let summary_path = std::env::temp_dir().join(format!(
            "file-server-ab-summary-{}.md",
            uuid::Uuid::new_v4()
        ));
        write_summary(&summary_path, "test-run", &report).expect("write summary");
        let summary = fs::read_to_string(&summary_path).expect("read summary");
        assert!(summary.contains("## Difference field summary (reporting only)"));
        assert!(summary.contains(
            "| HTTP | /http/headers/content-type | value | unclassified | 2 | header-a, header-b |"
        ));
        fs::remove_file(summary_path).expect("remove temporary summary");
    }

    #[test]
    fn expected_difference_rule_requires_exact_values() {
        let rules = RulesFile {
            schema_version: 1,
            rules: vec![DifferenceRule {
                case: "response-shape".to_string(),
                path: "/body/extra".to_string(),
                kind: "rust_only".to_string(),
                expected_rust: json!("rust-addition"),
                expected_typescript: json!({"$missing": true}),
                reason: "documented additive response field".to_string(),
                reviewed_by: "reviewer".to_string(),
                expires_on: "2099-12-31".to_string(),
            }],
        };
        validate_rules(&rules).expect("valid exact-value rule");

        let mut differences = vec![diff(
            "response-shape",
            "/body/extra",
            "rust_only",
            Some(json!("rust-addition")),
            None,
        )];
        apply_rules(&mut differences, &rules);
        assert!(differences[0].accepted);

        let mut changed = vec![diff(
            "response-shape",
            "/body/extra",
            "rust_only",
            Some(json!("unexpected-value")),
            None,
        )];
        apply_rules(&mut changed, &rules);
        assert!(!changed[0].accepted);
    }

    #[test]
    fn expected_difference_rules_cannot_waive_failed_assertions_or_transport_errors() {
        for kind in ["assertion_failed", "transport_error"] {
            let rules = RulesFile {
                schema_version: 1,
                rules: vec![DifferenceRule {
                    case: "case".to_string(),
                    path: "/http/status".to_string(),
                    kind: kind.to_string(),
                    expected_rust: json!(500),
                    expected_typescript: json!(500),
                    reason: "must remain a failing condition".to_string(),
                    reviewed_by: "reviewer".to_string(),
                    expires_on: "2099-12-31".to_string(),
                }],
            };

            assert!(
                validate_rules(&rules).is_err(),
                "rule kind {kind} was accepted"
            );
        }
    }

    #[test]
    fn named_file_arrays_compare_by_path_without_hiding_order_changes() {
        let rust = json!([
            {"name":"README.md","contents":"same"},
            {"name":"src/app.ts","contents":"same"}
        ]);
        let typescript = json!([
            {"name":"AGENTS.md","contents":"extra"},
            {"name":"README.md","contents":"same"},
            {"name":"src/app.ts","contents":"same"}
        ]);
        let mut differences = Vec::new();
        json_differences("files", "/files", &rust, &typescript, &[], &mut differences);
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].path, "/files/AGENTS.md");
        assert_eq!(differences[0].kind, "ts_only");

        let reordered = json!([
            {"name":"src/app.ts","contents":"same"},
            {"name":"README.md","contents":"same"}
        ]);
        differences.clear();
        json_differences("files", "/files", &rust, &reordered, &[], &mut differences);
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].path, "/files/$order");
        assert_eq!(differences[0].kind, "array_order");
    }

    #[test]
    fn git_diff_ignores_only_equivalent_single_line_hunk_count_formatting() {
        let rust = json!({
            "diff": "@@ -1,1 +1,1 @@\n-before\n+after\n@@ -0,0 +1,1 @@\n+new\n"
        });
        let typescript = json!({
            "diff": "@@ -1 +1 @@\n-before\n+after\n@@ -0,0 +1 @@\n+new\n"
        });
        let mut differences = Vec::new();
        json_differences("git-diff", "", &rust, &typescript, &[], &mut differences);
        assert!(differences.is_empty());

        let changed_content = json!({
            "diff": "@@ -1 +1 @@\n-before\n+different\n@@ -0,0 +1 @@\n+new\n"
        });
        json_differences(
            "git-diff",
            "",
            &rust,
            &changed_content,
            &[],
            &mut differences,
        );
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].path, "/diff");

        assert_ne!(
            normalize_git_diff_hunk_headers("@@ -1,2 +1,3 @@ context\n"),
            normalize_git_diff_hunk_headers("@@ -1 +1 @@ context\n")
        );
        assert_eq!(
            normalize_git_diff_hunk_headers("text,1 is not a hunk range\n"),
            "text,1 is not a hunk range\n"
        );
    }

    /// gix writes an empty top-of-file range as `-1,0` where system Git writes
    /// `-0,0`; that equivalence is verified only for whole-new-file hunks, so it
    /// applies exclusively inside `new file mode` blocks. Zero-line counts are never
    /// conflated with omitted one-line counts.
    #[test]
    fn git_diff_hunks_preserve_zero_counts_and_limit_empty_range_equivalence() {
        let gix_new_file = "diff --git a/src/new.txt b/src/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/src/new.txt\n@@ -1,0 +1,1 @@\n+new staged fixture\n";
        let git_new_file = "diff --git a/src/new.txt b/src/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/src/new.txt\n@@ -0,0 +1 @@\n+new staged fixture\n";
        // Observed producer difference for a whole-new-file hunk is equivalent.
        assert_eq!(
            normalize_git_diff_hunk_headers(gix_new_file),
            normalize_git_diff_hunk_headers(git_new_file)
        );
        // The same range difference outside a new-file block is not equivalent:
        // without verified context the raw ranges stay as produced.
        assert_ne!(
            normalize_git_diff_hunk_headers("@@ -1,0 +1 @@\n+new\n"),
            normalize_git_diff_hunk_headers("@@ -0,0 +1 @@\n+new\n")
        );
        // An empty range (zero lines) is never equal to an omitted one-line range.
        assert_ne!(
            normalize_git_diff_hunk_headers("@@ -2,0 +2 @@\n+new\n"),
            normalize_git_diff_hunk_headers("@@ -2 +2 @@\n+new\n")
        );
        // Inside a new-file block the zero count is still preserved explicitly.
        assert!(normalize_git_diff_hunk_headers(git_new_file).contains("@@ -0,0 +1 @@"));
        // Insertion positions of 2 and beyond keep distinguishing hunks.
        assert_ne!(
            normalize_git_diff_hunk_headers("@@ -3,0 +2 @@\n+after three\n"),
            normalize_git_diff_hunk_headers("@@ -0,0 +2 @@\n+at top\n")
        );
    }

    #[test]
    fn zip_responses_compare_by_entry_semantics_not_archive_order() {
        fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, body) in files {
                writer
                    .start_file(*name, zip::write::SimpleFileOptions::default())
                    .expect("start ZIP file");
                writer.write_all(body).expect("write ZIP file");
            }
            writer.finish().expect("finish ZIP").into_inner()
        }

        let ordered = archive(&[("README.md", b"readme"), ("src/main.ts", b"source")]);
        let reversed = archive(&[("src/main.ts", b"source"), ("README.md", b"readme")]);
        let changed = archive(&[("README.md", b"readme"), ("src/main.ts", b"different")]);

        assert_eq!(
            zip_semantic_entries(&ordered).expect("valid ZIP"),
            zip_semantic_entries(&reversed).expect("valid ZIP")
        );
        assert_ne!(
            zip_semantic_entries(&ordered).expect("valid ZIP"),
            zip_semantic_entries(&changed).expect("valid ZIP")
        );
        assert!(zip_semantic_entries(b"not a ZIP archive").is_err());
    }

    #[test]
    fn zip_semantics_ignore_implied_directories_but_preserve_empty_directories() {
        fn archive(explicit_parent_dirs: bool, include_empty_dir: bool) -> Vec<u8> {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default();
            if explicit_parent_dirs {
                writer
                    .add_directory("src/", options)
                    .expect("add src directory");
                writer
                    .add_directory("src/nested/", options)
                    .expect("add nested directory");
            }
            if include_empty_dir {
                writer
                    .add_directory("empty/", options)
                    .expect("add empty directory");
            }
            writer
                .start_file("src/nested/main.rs", options)
                .expect("start source file");
            writer.write_all(b"source").expect("write source file");
            writer.finish().expect("finish ZIP").into_inner()
        }

        let explicit = archive(true, true);
        let implicit = archive(false, true);
        let empty_missing = archive(false, false);

        assert_eq!(
            zip_semantic_entries(&explicit).expect("explicit directory ZIP"),
            zip_semantic_entries(&implicit).expect("implicit directory ZIP")
        );
        assert_ne!(
            zip_semantic_entries(&implicit).expect("implicit directory ZIP"),
            zip_semantic_entries(&empty_missing).expect("ZIP without empty directory")
        );
    }

    #[test]
    fn git_index_entries_are_ordered_by_path_not_object_id() {
        let entries = git_index_records(
            b"100644 ffffffffffffffffffffffffffffffffffffffff 0\tz.txt\0\
              100644 0000000000000000000000000000000000000000 0\ta.txt\0",
        );
        assert_eq!(
            entries,
            [
                "a.txt\t100644 0000000000000000000000000000000000000000 0",
                "z.txt\t100644 ffffffffffffffffffffffffffffffffffffffff 0"
            ]
        );
    }

    /// The dynamic-skill lock is a timestamp marker; equivalent installs at different
    /// times must not diff, while non-timestamp content still compares byte-wise.
    #[test]
    fn dynamic_add_lock_snapshots_compare_by_timestamp_shape() {
        assert!(is_timestamp_marker(b"1790387402896\n"));
        assert!(is_timestamp_marker(b"1790387402896"));
        assert!(!is_timestamp_marker(b""));
        assert!(!is_timestamp_marker(b"not-a-time\n"));
        assert!(!is_timestamp_marker(b"1790387402896\nextra"));
    }

    /// Generated `.npmrc` files carry a creation-second comment; everything else in
    /// the file must still compare byte-wise.
    #[test]
    fn generated_npmrc_normalizes_only_the_timestamp_line() {
        let rust = "# pnpm 优化配置\n# 自动生成于 2026-09-26 10:01:58\n# 文件系统类型: local\nregistry=https://registry.npmmirror.com\n";
        let typescript = "# pnpm 优化配置\n# 自动生成于 2026-09-26 10:01:57\n# 文件系统类型: local\nregistry=https://registry.npmmirror.com\n";
        let normalized_rust = normalize_generated_npmrc(rust.as_bytes()).expect("normalize");
        let normalized_ts = normalize_generated_npmrc(typescript.as_bytes()).expect("normalize");
        assert_eq!(normalized_rust, normalized_ts);
        assert!(normalized_rust.contains("# 自动生成于 <timestamp>"));

        let changed_registry =
            "# pnpm 优化配置\n# 自动生成于 2026-09-26 10:01:58\nregistry=https://example.com\n";
        let normalized_changed =
            normalize_generated_npmrc(changed_registry.as_bytes()).expect("normalize");
        assert_ne!(normalized_changed, normalized_rust);
        // Files without the generated stamp are not normalized at all.
        assert!(normalize_generated_npmrc(b"registry=x\n").is_none());
    }

    /// A final-phase snapshot failure must not discard completed cases: the partial
    /// report keeps every recorded case and the incomplete reason.
    #[test]
    fn final_snapshot_failure_preserves_completed_cases() {
        let report_dir =
            std::env::temp_dir().join(format!("file-server-ab-partial-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&report_dir).expect("create report dir");
        let journal = RequestJournal::new(report_dir.clone()).expect("journal");
        let mut recorder = Recorder::new(report_dir.clone(), Suite::Core, json!({"entries": []}))
            .expect("recorder");
        let case = executed_case("health", vec![]);
        recorder.record(&journal, case).expect("record case");
        let rules = RulesFile {
            schema_version: 1,
            rules: Vec::new(),
        };
        // Poison the workspace root: a FILE where snapshot_roots expects a directory,
        // so the best-effort final snapshot fails while the report is still written.
        let poisoned_root =
            std::env::temp_dir().join(format!("file-server-ab-poison-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&poisoned_root).expect("create poisoned root");
        fs::write(poisoned_root.join("project-workspace"), b"not a directory")
            .expect("poison snapshot root");
        recorder.write_incomplete(
            &journal,
            "test-run",
            &rules,
            &poisoned_root,
            &poisoned_root,
            "final snapshot failed",
        );
        let summary = fs::read_to_string(report_dir.join("summary.md")).expect("summary written");
        assert!(summary.contains("Incomplete run"));
        assert!(summary.contains("`health`"));
        let diff: Value =
            serde_json::from_str(&fs::read_to_string(report_dir.join("diff.json")).expect("diff"))
                .expect("partial diff json");
        assert_eq!(
            diff["summary"]["incomplete_reason"],
            "final snapshot failed"
        );
        assert_eq!(diff["cases"][0]["case"], "health");
        drop(fs::remove_dir_all(report_dir));
        drop(fs::remove_dir_all(poisoned_root));
    }

    /// Minimal HTTP responder used to drive the gating logic against real sockets.
    async fn spawn_test_server(
        status: u16,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_writer = seen.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let seen_writer = seen_writer.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // Read until the end of headers (test requests carry no body).
                    loop {
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buffer);
                    let path = head
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string();
                    seen_writer.lock().unwrap().push(path);
                    let body = format!("{{\"success\":{}}}", status == 200);
                    let response = format!(
                        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        if status == 200 {
                            "OK"
                        } else {
                            "Internal Server Error"
                        },
                        body.len()
                    );
                    drop(socket.write_all(response.as_bytes()).await);
                });
            }
        });
        (format!("http://{addr}"), seen, handle)
    }

    /// Execution-level failure isolation over real sockets: an upstream scenario whose
    /// TypeScript side fails stops its dependent from sending ANY request (both
    /// servers observe nothing for it), while an independent scenario still executes
    /// against both servers.
    #[tokio::test]
    async fn upstream_failure_blocks_dependents_and_independent_projects_continue() {
        let (rust_url, rust_seen, _rust_handle) = spawn_test_server(200).await;
        let (ts_url, ts_seen, _ts_handle) = spawn_test_server(500).await;
        let report_dir =
            std::env::temp_dir().join(format!("file-server-ab-isolation-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&report_dir).expect("create report dir");
        let client = client().expect("http client");
        let mut journal = RequestJournal::new(report_dir.clone()).expect("journal");
        let mut recorder = Recorder::new(report_dir.clone(), Suite::Core, json!({"entries": []}))
            .expect("recorder");

        // Upstream: TypeScript answers 500, so the case fails its status contract.
        let upload = get_spec("/api/computer/upload-file".to_string());
        let upload_result = run_gated_pair(
            &client,
            &mut journal,
            &mut recorder,
            "computer-upload-file-binary",
            &rust_url,
            &ts_url,
            &upload,
            &upload,
            RequestTarget::Api,
        )
        .await
        .expect("upload stage")
        .expect("upload is not gated");
        assert!(
            case_failed(&upload_result.0),
            "the 500 response must fail the upstream case"
        );
        recorder
            .record(&journal, upload_result.0)
            .expect("record upload");

        // Dependent read scenario must be blocked without any request leaving.
        let dependent = get_spec("/api/computer/static/u/s/ab.bin".to_string());
        let dependent_result = run_gated_pair(
            &client,
            &mut journal,
            &mut recorder,
            "computer-read-uploaded-single-binary",
            &rust_url,
            &ts_url,
            &dependent,
            &dependent,
            RequestTarget::Api,
        )
        .await
        .expect("dependent stage");
        assert!(dependent_result.is_none(), "dependent must be blocked");
        let paths =
            |seen: &std::sync::Arc<std::sync::Mutex<Vec<String>>>| seen.lock().unwrap().clone();
        for seen in [&rust_seen, &ts_seen] {
            assert!(
                !paths(seen)
                    .iter()
                    .any(|p| p.contains("/api/computer/static")),
                "blocked dependent must not send requests, saw {:?}",
                paths(seen)
            );
        }

        // An independent scenario in the same run still executes on both sides.
        let independent = get_spec("/api/computer/generate-file".to_string());
        let independent_result = run_gated_pair(
            &client,
            &mut journal,
            &mut recorder,
            "computer-generate-file-utf8",
            &rust_url,
            &ts_url,
            &independent,
            &independent,
            RequestTarget::Api,
        )
        .await
        .expect("independent stage")
        .expect("independent scenario is not gated");
        recorder
            .record(&journal, independent_result.0)
            .expect("record independent");
        assert!(
            paths(&rust_seen)
                .iter()
                .any(|p| p.contains("generate-file"))
        );
        assert!(paths(&ts_seen).iter().any(|p| p.contains("generate-file")));

        drop(fs::remove_dir_all(report_dir));
    }

    /// Protocol-level header equivalence: charset presence, opaque etags,
    /// derived content lengths, and static-content date validators.
    #[test]
    fn header_protocol_equivalence_is_validated_not_ignored() {
        use HeaderVerdict::*;
        let eq = |a, b, c, d, e, f| header_protocol_verdict(a, b, c, d, e, f);

        // content-type: same media type, charset stated vs implicit -> equivalent;
        // different charsets or media types -> different.
        assert!(matches!(
            eq(
                "content-type",
                Some("application/json"),
                Some("application/json; charset=utf-8"),
                false,
                0,
                0
            ),
            Equivalent
        ));
        assert!(matches!(
            eq(
                "content-type",
                Some("text/plain"),
                Some("text/plain; charset=UTF-8"),
                false,
                0,
                0
            ),
            Equivalent
        ));
        assert!(matches!(
            eq(
                "content-type",
                Some("application/json"),
                Some("text/plain"),
                false,
                0,
                0
            ),
            Different
        ));
        assert!(matches!(
            eq(
                "content-type",
                Some("text/plain; charset=utf-8"),
                Some("text/plain; charset=iso-8859-1"),
                false,
                0,
                0
            ),
            Different
        ));
        // Malformed media type is a per-side failure, not a waiver.
        assert!(matches!(
            eq(
                "content-type",
                Some("not-a-media-type"),
                Some("application/json"),
                false,
                0,
                0
            ),
            Invalid { side: "rust", .. }
        ));

        // etag: two well-formed tags are equivalent (opaque); malformed tags fail
        // per side; on static content a presence mismatch stays a real difference,
        // elsewhere Express's framework-default etag vs none is equivalent.
        assert!(matches!(
            eq("etag", Some("W/\"22d-a\""), Some("\"abc\""), false, 0, 0),
            Equivalent
        ));
        assert!(matches!(
            eq("etag", Some("no-quotes"), Some("\"abc\""), false, 0, 0),
            Invalid { side: "rust", .. }
        ));
        assert!(matches!(
            eq("etag", None, Some("W/\"22d-a\""), true, 0, 0),
            Different
        ));
        assert!(matches!(
            eq("etag", None, Some("W/\"22d-a\""), false, 0, 0),
            Equivalent
        ));

        // content-length: each side must match its own body; then equivalent.
        assert!(matches!(
            eq("content-length", Some("10"), Some("20"), false, 10, 20),
            Equivalent
        ));
        assert!(matches!(
            eq("content-length", Some("10"), Some("20"), false, 11, 20),
            Invalid { side: "rust", .. }
        ));
        assert!(matches!(
            eq("content-length", Some("10"), Some("xx"), false, 10, 20),
            Invalid {
                side: "typescript",
                ..
            }
        ));
        assert!(matches!(
            eq("content-length", None, Some("20"), false, 0, 20),
            Different
        ));

        // last-modified: static fixture mtimes differ per side but both must be
        // HTTP-dates; non-static values compare strictly.
        assert!(matches!(
            eq(
                "last-modified",
                Some("Sat, 26 Sep 2026 02:40:08 GMT"),
                Some("Sat, 26 Sep 2026 02:40:00 GMT"),
                true,
                0,
                0
            ),
            Equivalent
        ));
        assert!(matches!(
            eq(
                "last-modified",
                Some("Sat, 26 Sep 2026 02:40:08 GMT"),
                Some("not a date"),
                true,
                0,
                0
            ),
            Invalid {
                side: "typescript",
                ..
            }
        ));
        assert!(matches!(
            eq(
                "last-modified",
                Some("Sat, 26 Sep 2026 02:40:08 GMT"),
                Some("Sat, 26 Sep 2026 02:40:00 GMT"),
                false,
                0,
                0
            ),
            Different
        ));

        // Unlisted headers always compare strictly.
        assert!(matches!(
            eq("cache-control", None, Some("public, max-age=0"), true, 0, 0),
            Different
        ));
    }

    /// The dev-log page contract rejects structurally broken pages even though the
    /// log text itself is normalized.
    #[test]
    fn log_page_contract_rejects_broken_pages() {
        let good = json!({
            "success": true, "startIndex": 1, "totalLines": 3,
            "logFileName": "dev-temp-1790389495012.log",
            "logs": [
                {"line": 1, "content": "a"},
                {"line": 2, "content": ""},
                {"line": 3, "content": "c"},
            ]
        });
        let body = serde_json::to_vec(&good).unwrap();
        assert_eq!(validate_log_page(&body, 1), Ok((3, 3)));

        // Codex 反例: 缺正文、null 行、非法/乱序行号、文件名与 totalLines 不符。
        let broken = json!({
            "success": true, "startIndex": 1, "totalLines": 999999,
            "logFileName": "",
            "logs": [{"line": 1}, null, {"line": -99}]
        });
        let body = serde_json::to_vec(&broken).unwrap();
        assert!(validate_log_page(&body, 1).is_err());

        let missing_content = json!({
            "success": true, "startIndex": 1, "totalLines": 1,
            "logFileName": "dev-temp-1.log",
            "logs": [{"line": 1, "content": 42}]
        });
        assert!(validate_log_page(&serde_json::to_vec(&missing_content).unwrap(), 1).is_err());

        let non_consecutive = json!({
            "success": true, "startIndex": 1, "totalLines": 5,
            "logFileName": "dev-temp-1.log",
            "logs": [{"line": 1, "content": "a"}, {"line": 3, "content": "b"}]
        });
        assert!(validate_log_page(&serde_json::to_vec(&non_consecutive).unwrap(), 1).is_err());

        let start_echo = json!({
            "success": true, "startIndex": 2, "totalLines": 3,
            "logFileName": "dev-temp-1.log",
            "logs": [{"line": 1, "content": "a"}, {"line": 2, "content": "b"}]
        });
        assert!(validate_log_page(&serde_json::to_vec(&start_echo).unwrap(), 1).is_err());

        let empty_first_page = json!({
            "success": true, "startIndex": 1, "totalLines": 0,
            "logFileName": "dev-temp-1.log", "logs": []
        });
        assert!(validate_log_page(&serde_json::to_vec(&empty_first_page).unwrap(), 1).is_err());

        let short_total = json!({
            "success": true, "startIndex": 1, "totalLines": 1,
            "logFileName": "dev-temp-1.log",
            "logs": [{"line": 1, "content": "a"}, {"line": 2, "content": "b"}]
        });
        assert!(validate_log_page(&serde_json::to_vec(&short_total).unwrap(), 1).is_err());

        // A past-the-end second page is valid: empty, echoing start, total unchanged.
        let tail_page = json!({
            "success": true, "startIndex": 4, "totalLines": 3,
            "logFileName": "dev-temp-1.log", "logs": []
        });
        assert_eq!(
            validate_log_page(&serde_json::to_vec(&tail_page).unwrap(), 4),
            Ok((3, 3))
        );
    }

    #[tokio::test]
    async fn dev_port_probe_distinguishes_open_from_refused_ports() {
        let open_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind open probe listener");
        let open_endpoint = format!(
            "http://{}",
            open_listener.local_addr().expect("open listener addr")
        );

        assert!(
            dev_port_accepts_connections(&open_endpoint)
                .await
                .expect("probe listening port")
        );

        let closed_endpoint = "http://127.0.0.1:1";
        assert!(
            !dev_port_accepts_connections(closed_endpoint)
                .await
                .expect("probe closed port"),
            "closed endpoint {closed_endpoint} must refuse connections"
        );
    }
}
