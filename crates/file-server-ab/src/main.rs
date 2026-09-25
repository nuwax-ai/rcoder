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
    elapsed_ms: u128,
    transport_error: Option<String>,
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

#[derive(Debug, Serialize)]
struct CaseResult {
    case: String,
    equal: bool,
    compared: String,
    normalized_paths: Vec<String>,
    normalized_headers: Vec<String>,
    rust_status: Option<u16>,
    ts_status: Option<u16>,
    differences: Vec<Difference>,
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

#[derive(Debug, Serialize)]
struct Summary {
    comparisons: usize,
    equal: usize,
    expected_differences: usize,
    unclassified_differences: usize,
    environment_errors: usize,
    environment_blocked: bool,
    precondition_errors: usize,
}

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
}

impl ExpectedStatus {
    fn matches(self, status: Option<StatusCode>) -> bool {
        status.is_some_and(|status| match self {
            Self::Success2xx => status.is_success(),
            Self::ClientError4xx => status.is_client_error(),
            Self::PartialContent206 => status == StatusCode::PARTIAL_CONTENT,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::Success2xx => "2xx",
            Self::ClientError4xx => "4xx",
            Self::PartialContent206 => "206",
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
        let executed = cases.iter().filter_map(Value::as_str).any(|case| {
            matches!(suite, Suite::All)
                || match suite {
                    Suite::Core => !case.starts_with("git-") && !case.starts_with("build-"),
                    Suite::Git => case.starts_with("git-"),
                    Suite::Build => case.starts_with("build-"),
                    Suite::All => true,
                }
        });
        if let Some(object) = entry.as_object_mut() {
            object.insert("executed_in_this_run".into(), json!(executed));
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
        configuration_profile: "isolated-compose-containers".to_string(),
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
    let mut requests = fs::File::create(report_dir.join("requests.jsonl"))?;
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
                environment_errors: errors.len(),
                environment_blocked: true,
                precondition_errors: 0,
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
                environment_errors: 0,
                environment_blocked: false,
                precondition_errors: initial_state_differences.len(),
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

    let mut cases = Vec::new();
    if matches!(suite, Suite::Core | Suite::All) {
        for (case_name, spec) in core_scenarios(&fixtures)? {
            let (mut case, rust, ts) = run_pair_specs(
                &client,
                &report_dir,
                &mut requests,
                case_name,
                &rust_url,
                &ts_url,
                &spec,
                &spec,
            )
            .await?;
            for (side, exchange) in [("rust", &rust), ("typescript", &ts)] {
                let validation = match case_name {
                    "computer-file-meta" => validate_meta_response(&exchange.body),
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
                    | "project-upload-project-wrapper-zip" => validate_success_json(&exchange.body),
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
                    "computer-import-preservation-contract" => {
                        validate_execute_command_output(&exchange.body, "import preservation ok\n")
                    }
                    "computer-init-template-git-tree" => validate_git_tree_response(&exchange.body),
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
                    "computer-zip-workspace-semantic" => validate_workspace_archive(&exchange.body),
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
            cases.push(case);
        }
    }
    if matches!(suite, Suite::Git | Suite::All) {
        let mut rust_commit_hash = None;
        let mut ts_commit_hash = None;
        for scenario in git_scenarios()? {
            let case_name = scenario.name.as_str();
            if let Some(preparation) = scenario.preparation {
                prepare_git_scenario(preparation, &scenario.project_id, &rust_root, &ts_root)?;
            }
            let (mut case, rust, ts) = run_pair_specs(
                &client,
                &report_dir,
                &mut requests,
                case_name,
                &rust_url,
                &ts_url,
                &scenario.spec,
                &scenario.spec,
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
            cases.push(case);
        }
    }
    if matches!(suite, Suite::Build | Suite::All) {
        if rust_dev_url.trim().is_empty() || ts_dev_url.trim().is_empty() {
            bail!("build suite requires --rust-dev-url and --ts-dev-url");
        }
        let build_cases = run_build_suite(
            &client,
            &report_dir,
            &mut requests,
            &rust_url,
            &ts_url,
            &rust_dev_url,
            &ts_dev_url,
        )
        .await?;
        for case in build_cases {
            println!("{} {}", if case.equal { "PASS" } else { "DIFF" }, case.case);
            cases.push(case);
        }
    }

    let rust_state = snapshot_roots(&rust_root)?;
    let ts_state = snapshot_roots(&ts_root)?;
    write_json(&report_dir.join("state/rust.json"), &rust_state)?;
    write_json(&report_dir.join("state/typescript.json"), &ts_state)?;
    let mut state_differences = compare_snapshots(&rust_state, &ts_state)?;
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
    for case in &mut cases {
        apply_rules(&mut case.differences, &rules);
        case.equal = case.differences.is_empty();
    }
    apply_rules(&mut state_differences, &rules);
    apply_rules(&mut git_state_differences, &rules);

    let equal = cases.iter().filter(|case| case.equal).count()
        + usize::from(initial_state_differences.is_empty())
        + usize::from(state_differences.is_empty())
        + usize::from(git_suite_ran && git_state_differences.is_empty());
    let expected_differences = cases
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
    let unclassified_differences = cases
        .iter()
        .flat_map(|case| &case.differences)
        .filter(|difference| !difference.accepted && difference.kind != "transport_error")
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
    let environment_errors = cases
        .iter()
        .flat_map(|case| &case.differences)
        .filter(|difference| difference.kind == "transport_error")
        .count();
    let diff = RunDiff {
        summary: Summary {
            comparisons: cases.len() + 2 + usize::from(git_suite_ran),
            equal,
            expected_differences,
            unclassified_differences,
            environment_errors,
            environment_blocked: environment_errors > 0,
            precondition_errors: 0,
        },
        cases,
        initial_state_differences,
        state_differences,
        git_state_differences,
    };
    write_json(&report_dir.join("diff.json"), &diff)?;
    write_summary(&report_dir.join("summary.md"), &run_id, &diff)?;
    println!("report: {}", report_dir.display());
    println!(
        "summary: {}/{} equal; {} expected; {} unclassified",
        diff.summary.equal,
        diff.summary.comparisons,
        diff.summary.expected_differences,
        diff.summary.unclassified_differences
    );
    if diff.summary.unclassified_differences > 0 {
        bail!("A/B run contains unclassified differences; inspect diff.json and summary.md");
    }
    if diff.summary.environment_blocked {
        bail!(
            "A/B run was blocked by HTTP transport errors; inspect requests.jsonl and summary.md"
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
    let mut static_read =
        get("/api/page/static/file-server-ab-react/src/file-server-ab.txt".to_string());
    static_read.normalized_headers = vec!["etag".into(), "last-modified".into()];
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
        ("version", get("/api/version".to_string())),
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
                    { "name": "empty.txt", "contents": "", "binary": false }
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
    scenarios.push((
        "project-export-latest-semantic-zip",
        json_spec(
            Method::POST,
            "/api/project/export-project",
            json!({ "projectId": project, "codeVersion": "4", "exportType": "LATEST" }),
        )?,
    ));
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

async fn run_build_suite(
    client: &Client,
    report_dir: &Path,
    requests: &mut fs::File,
    rust_api_url: &str,
    ts_api_url: &str,
    rust_dev_url: &str,
    ts_dev_url: &str,
) -> Result<Vec<CaseResult>> {
    let mut results = Vec::new();

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
        report_dir,
        requests,
        parse_case,
        rust_api_url,
        ts_api_url,
        &parse_spec,
        &parse_spec,
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
    results.push(parse_result);

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
            report_dir,
            requests,
            &create_case,
            rust_api_url,
            ts_api_url,
            &create_spec,
            &create_spec,
        )
        .await?;
        results.push(case);

        let build_case = format!("build-{template_type}-production-build");
        let mut build_spec = get_spec(format!(
            "/api/build/build?projectId={project_id}&basePath=%2F"
        ));
        build_spec.timeout = Duration::from_secs(720);
        let (case, _, _) = run_pair_specs(
            client,
            report_dir,
            requests,
            &build_case,
            rust_api_url,
            ts_api_url,
            &build_spec,
            &build_spec,
        )
        .await?;
        results.push(case);

        let artifact_case = format!("build-{template_type}-static-dist-index");
        let mut artifact_spec = get_spec(format!("/api/page/static/{project_id}/dist/index.html"));
        artifact_spec.normalized_headers = vec!["etag".into(), "last-modified".into()];
        let (case, _, _) = run_pair_specs(
            client,
            report_dir,
            requests,
            &artifact_case,
            rust_api_url,
            ts_api_url,
            &artifact_spec,
            &artifact_spec,
        )
        .await?;
        results.push(case);

        let start_case = format!("build-{template_type}-start-dev");
        let mut start_spec = get_spec(format!(
            "/api/build/start-dev?projectId={project_id}&basePath=%2F"
        ));
        start_spec.normalized_paths = vec!["/pid".into(), "/port".into()];
        start_spec.timeout = Duration::from_secs(720);
        let (mut case, rust_start, ts_start) = run_pair_specs(
            client,
            report_dir,
            requests,
            &start_case,
            rust_api_url,
            ts_api_url,
            &start_spec,
            &start_spec,
        )
        .await?;
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
                results.push(case);

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
                let (case, _, _) = run_pair_specs(
                    client,
                    report_dir,
                    requests,
                    &probe_case,
                    &rust_dev_endpoint,
                    &ts_dev_endpoint,
                    &probe_spec,
                    &probe_spec,
                )
                .await?;
                results.push(case);

                if template_type == "react" {
                    let log_case = "build-react-get-dev-log";
                    let log_spec = get_spec(format!(
                        "/api/build/get-dev-log?projectId={project_id}&startIndex=1&logType=temp"
                    ));
                    let (mut log_result, rust_log, ts_log) = run_pair_specs(
                        client,
                        report_dir,
                        requests,
                        log_case,
                        rust_api_url,
                        ts_api_url,
                        &log_spec,
                        &log_spec,
                    )
                    .await?;
                    for (side, response) in [("rust", &rust_log), ("typescript", &ts_log)] {
                        let value = serde_json::from_slice::<Value>(&response.body).ok();
                        let valid = value.as_ref().is_some_and(|body| {
                            let logs = body.get("logs").and_then(Value::as_array);
                            let total_lines = body.get("totalLines").and_then(Value::as_u64);
                            body.get("success").and_then(Value::as_bool) == Some(true)
                                && logs.is_some_and(|logs| {
                                    !logs.is_empty()
                                        && logs[0].get("line").and_then(Value::as_u64) == Some(1)
                                        && total_lines
                                            .is_some_and(|total| total >= logs.len() as u64)
                                })
                                && body.get("startIndex").and_then(Value::as_u64) == Some(1)
                        });
                        if !valid {
                            log_result.differences.push(assertion_difference(
                                log_case,
                                &format!("/assertions/{side}/log-page"),
                                "expected a successful non-empty log page starting at line 1 with a consistent totalLines count".into(),
                            ));
                        }
                    }
                    results.push(log_result);

                    let stats_case = "build-react-log-cache-stats";
                    let stats_spec = get_spec("/api/build/get-log-cache-stats");
                    let (mut stats_result, rust_stats, ts_stats) = run_pair_specs(
                        client,
                        report_dir,
                        requests,
                        stats_case,
                        rust_api_url,
                        ts_api_url,
                        &stats_spec,
                        &stats_spec,
                    )
                    .await?;
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
                    results.push(stats_result);

                    let clear_case = "build-react-clear-log-cache";
                    let clear_spec = get_spec("/api/build/clear-all-log-cache");
                    let (mut clear_result, rust_clear, ts_clear) = run_pair_specs(
                        client,
                        report_dir,
                        requests,
                        clear_case,
                        rust_api_url,
                        ts_api_url,
                        &clear_spec,
                        &clear_spec,
                    )
                    .await?;
                    for (side, response) in [("rust", &rust_clear), ("typescript", &ts_clear)] {
                        if !json_bool(&response.body, "success") {
                            clear_result.differences.push(assertion_difference(
                                clear_case,
                                &format!("/assertions/{side}/success"),
                                "expected success=true after clearing log cache".into(),
                            ));
                        }
                    }
                    results.push(clear_result);

                    let cleared_stats_case = "build-react-log-cache-stats-after-clear";
                    let cleared_stats_spec = get_spec("/api/build/get-log-cache-stats");
                    let (mut cleared_stats_result, rust_cleared_stats, ts_cleared_stats) =
                        run_pair_specs(
                            client,
                            report_dir,
                            requests,
                            cleared_stats_case,
                            rust_api_url,
                            ts_api_url,
                            &cleared_stats_spec,
                            &cleared_stats_spec,
                        )
                        .await?;
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
                    results.push(cleared_stats_result);

                    let pool_case = "build-react-port-pool-status";
                    let mut pool_spec = get_spec("/api/build/port-pool-status");
                    // Both isolated services allocate different concrete ports. Validate each
                    // allocation against that side's start-dev response below, then compare
                    // the remaining port-pool contract normally.
                    pool_spec.normalized_paths = vec!["/allocations/0/port".into()];
                    let (mut pool_result, rust_pool, ts_pool) = run_pair_specs(
                        client,
                        report_dir,
                        requests,
                        pool_case,
                        rust_api_url,
                        ts_api_url,
                        &pool_spec,
                        &pool_spec,
                    )
                    .await?;
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
                    results.push(pool_result);
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
                let (case, _, _) = run_pair_specs(
                    client,
                    report_dir,
                    requests,
                    &keep_case,
                    rust_api_url,
                    ts_api_url,
                    &rust_keep,
                    &ts_keep,
                )
                .await?;
                results.push(case);

                let restart_case = format!("build-{template_type}-restart-dev");
                let mut restart_spec = get_spec(format!(
                    "/api/build/restart-dev?projectId={project_id}&basePath=%2F"
                ));
                restart_spec.normalized_paths = vec!["/pid".into(), "/port".into()];
                restart_spec.timeout = Duration::from_secs(720);
                let (mut case, rust_restart, ts_restart) = run_pair_specs(
                    client,
                    report_dir,
                    requests,
                    &restart_case,
                    rust_api_url,
                    ts_api_url,
                    &restart_spec,
                    &restart_spec,
                )
                .await?;
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
                        results.push(case);
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
                        let (case, _, _) = run_pair_specs(
                            client,
                            report_dir,
                            requests,
                            &probe_case,
                            &rust_dev_endpoint,
                            &ts_dev_endpoint,
                            &probe_spec,
                            &probe_spec,
                        )
                        .await?;
                        results.push(case);

                        let stop_case = format!("build-{template_type}-stop-dev");
                        let rust_stop = get_spec(format!(
                            "/api/build/stop-dev?projectId={project_id}&pid={}",
                            rust_server.pid
                        ));
                        let ts_stop = get_spec(format!(
                            "/api/build/stop-dev?projectId={project_id}&pid={}",
                            ts_server.pid
                        ));
                        let (mut case, rust_stop_response, ts_stop_response) = run_pair_specs(
                            client,
                            report_dir,
                            requests,
                            &stop_case,
                            rust_api_url,
                            ts_api_url,
                            &rust_stop,
                            &ts_stop,
                        )
                        .await?;
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
                        results.push(case);

                        let list_case = format!("build-{template_type}-list-after-stop");
                        let list_spec = get_spec("/api/build/list-dev".to_string());
                        let (mut case, rust_list, ts_list) = run_pair_specs(
                            client,
                            report_dir,
                            requests,
                            &list_case,
                            rust_api_url,
                            ts_api_url,
                            &list_spec,
                            &list_spec,
                        )
                        .await?;
                        for (side, response) in [("rust", &rust_list), ("typescript", &ts_list)] {
                            if response_has_project(&response.body, project_id) {
                                case.differences.push(assertion_difference(
                                    &list_case,
                                    &format!("/assertions/{side}/project-stopped"),
                                    format!("project {project_id} remains in list-dev after stop"),
                                ));
                            }
                        }
                        results.push(case);
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
                        results.push(case);
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
                results.push(case);
            }
        }
    }
    Ok(results)
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
    report_dir: &Path,
    requests: &mut fs::File,
    case: &str,
    rust_url: &str,
    ts_url: &str,
    rust_spec: &RequestSpec,
    ts_spec: &RequestSpec,
) -> Result<(CaseResult, Exchange, Exchange)> {
    let ts = exchange(
        client,
        report_dir,
        requests,
        case,
        "typescript",
        ts_url,
        ts_spec,
    )
    .await?;
    let rust = exchange(
        client, report_dir, requests, case, "rust", rust_url, rust_spec,
    )
    .await?;
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
    let differences = compare_exchange(
        case,
        &rust,
        &ts,
        health_probe,
        rust_spec.expected_status,
        &normalized_paths,
        &normalized_headers,
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
    };
    Ok((case_result, rust, ts))
}

async fn exchange(
    client: &Client,
    report_dir: &Path,
    requests: &mut fs::File,
    case: &str,
    side: &str,
    base_url: &str,
    spec: &RequestSpec,
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
        let (file, truncated) = save_body(report_dir, case, side, "request", &spec.body)?;
        (Some(file), truncated)
    };
    let started = std::time::Instant::now();
    let response = request.timeout(spec.timeout).send().await;
    let elapsed_ms = started.elapsed().as_millis();
    let exchange = match response {
        Ok(response) => {
            let status = response.status();
            let headers = response.headers().clone();
            let body = match response.bytes().await {
                Ok(body) => body.to_vec(),
                Err(error) => {
                    let message = format!("read response body: {error:#}");
                    let record = RequestLine {
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
                        elapsed_ms,
                        transport_error: Some(message.clone()),
                    };
                    append_jsonl(requests, &record)?;
                    return Ok(Exchange {
                        status: Some(status),
                        headers,
                        body: Vec::new(),
                        transport_error: Some(message),
                    });
                }
            };
            let (response_body_file, response_body_truncated) =
                save_body(report_dir, case, side, "response", &body)?;
            let record = RequestLine {
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
                elapsed_ms,
                transport_error: None,
            };
            append_jsonl(requests, &record)?;
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
                elapsed_ms,
                transport_error: Some(message.clone()),
            };
            append_jsonl(requests, &record)?;
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

fn compare_exchange(
    case: &str,
    rust: &Exchange,
    ts: &Exchange,
    health_probe: bool,
    expected_status: ExpectedStatus,
    normalized_paths: &[String],
    normalized_headers: &[String],
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
        if rust_value != ts_value {
            differences.push(diff(
                case,
                &format!("/http/headers/{header}"),
                "value",
                rust_value.map(|value| json!(value)),
                ts_value.map(|value| json!(value)),
            ));
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
        let semantic = json!({
            "kind": kind,
            "sha256": if kind == "directory" { None } else { Some(sha256(&content)) },
            "size_bytes": if kind == "directory" { None } else { Some(content.len()) },
            "mode": mode.map(|mode| format!("{:04o}", mode & 0o7777)),
        });
        if entries.insert(name.clone(), semantic).is_some() {
            bail!("ZIP contains duplicate entry path {name:?}");
        }
    }
    serde_json::to_value(entries).context("serialize ZIP semantic entries")
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
            entries.push(SnapshotEntry {
                path: relative,
                kind: "file".into(),
                sha256: Some(sha256(&contents)),
                size_bytes: Some(contents.len() as u64),
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
            if case.equal { "equal" } else { "different" },
            display_status(case.rust_status),
            display_status(case.ts_status),
            case.differences.len()
        )?;
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
}
