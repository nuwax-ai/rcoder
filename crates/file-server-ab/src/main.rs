use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
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
struct RunDiff {
    cases: Vec<CaseResult>,
    initial_state_differences: Vec<Difference>,
    state_differences: Vec<Difference>,
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
        prepare_git_fixture(&rust_root)?;
        prepare_git_fixture(&ts_root)?;
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
        for (case_name, spec) in core_scenarios()? {
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
            if case_name == "computer-file-meta" {
                for (side, exchange) in [("rust", &rust), ("typescript", &ts)] {
                    if let Err(error) = validate_meta_response(&exchange.body) {
                        case.differences.push(assertion_difference(
                            case_name,
                            &format!("/assertions/{side}/metadata"),
                            error,
                        ));
                    }
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
        for (case_name, spec) in git_scenarios()? {
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
    for case in &mut cases {
        apply_rules(&mut case.differences, &rules);
        case.equal = case.differences.is_empty();
    }
    apply_rules(&mut state_differences, &rules);

    let equal = cases.iter().filter(|case| case.equal).count()
        + usize::from(initial_state_differences.is_empty())
        + usize::from(state_differences.is_empty());
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
            .count();
    let environment_errors = cases
        .iter()
        .flat_map(|case| &case.differences)
        .filter(|difference| difference.kind == "transport_error")
        .count();
    let diff = RunDiff {
        summary: Summary {
            comparisons: cases.len() + 2,
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

fn core_scenarios() -> Result<Vec<(&'static str, RequestSpec)>> {
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
    let mut static_read =
        get("/api/page/static/file-server-ab-react/src/file-server-ab.txt".to_string());
    static_read.normalized_headers = vec!["etag".into(), "last-modified".into()];
    let mut static_range = get("/api/page/static/file-server-ab-react/package.json".to_string());
    static_range
        .headers
        .insert("range".into(), "bytes=0-5".into());
    static_range.expected_status = ExpectedStatus::PartialContent206;
    Ok(vec![
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
            json_request(
                Method::POST,
                "/api/project/create-project".to_string(),
                json!({"projectId":"file-server-ab-react", "templateType":"react"}),
            )?,
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
            json_request(
                Method::POST,
                "/api/project/create-project".to_string(),
                json!({"projectId":"file-server-ab-vue", "templateType":"vue3"}),
            )?,
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
            "computer-search-files",
            get(format!(
                "/api/computer/search-files?userId={user}&cId={cid}&kw=hello&limit=10&maxVisit=100&timeoutMs=1000"
            )),
        ),
        ("computer-file-meta", file_meta),
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
    ])
}

fn git_scenarios() -> Result<Vec<(&'static str, RequestSpec)>> {
    let base = json!({
        "workspaceType":"pageApp",
        "projectId":"file-server-ab-git"
    });
    let mut git_log =
        get_spec("/api/git/log?workspaceType=pageApp&projectId=file-server-ab-git&maxCount=10");
    git_log.normalized_paths = vec!["/commits/0/hash".into(), "/commits/0/date".into()];
    Ok(vec![
        (
            "git-init",
            json_spec(Method::POST, "/api/git/init", base.clone())?,
        ),
        (
            "git-status-before-first-commit",
            get_spec("/api/git/status?workspaceType=pageApp&projectId=file-server-ab-git"),
        ),
        (
            "git-add-initial-files",
            json_spec(
                Method::POST,
                "/api/git/add",
                json!({"workspaceType":"pageApp","projectId":"file-server-ab-git","files":["README.md","src/index.html"]}),
            )?,
        ),
        (
            "git-commit-initial",
            json_spec(
                Method::POST,
                "/api/git/commit",
                json!({
                    "workspaceType":"pageApp",
                    "projectId":"file-server-ab-git",
                    "message":"A/B initial commit",
                    "authorName":"File Server A-B",
                    "authorEmail":"ab@example.invalid"
                }),
            )?,
        ),
        (
            "git-status-clean",
            get_spec("/api/git/status?workspaceType=pageApp&projectId=file-server-ab-git"),
        ),
        (
            "git-read-head-file",
            json_spec(
                Method::POST,
                "/api/git/file-content",
                json!({"workspaceType":"pageApp","projectId":"file-server-ab-git","filePath":"src/index.html","ref":"HEAD"}),
            )?,
        ),
        (
            "git-create-branch",
            json_spec(
                Method::POST,
                "/api/git/branch-create",
                json!({"workspaceType":"pageApp","projectId":"file-server-ab-git","branchName":"ab-review"}),
            )?,
        ),
        (
            "git-list-branches",
            get_spec("/api/git/branches?workspaceType=pageApp&projectId=file-server-ab-git"),
        ),
        (
            "git-switch-main",
            json_spec(
                Method::POST,
                "/api/git/branch-switch",
                json!({"workspaceType":"pageApp","projectId":"file-server-ab-git","branchName":"main"}),
            )?,
        ),
        (
            "git-create-tag",
            json_spec(
                Method::POST,
                "/api/git/tag-create",
                json!({"workspaceType":"pageApp","projectId":"file-server-ab-git","tagName":"ab-v1","message":"A/B baseline tag"}),
            )?,
        ),
        (
            "git-list-tags",
            get_spec("/api/git/tags?workspaceType=pageApp&projectId=file-server-ab-git"),
        ),
        (
            "git-delete-tag",
            json_spec(
                Method::POST,
                "/api/git/tag-delete",
                json!({"workspaceType":"pageApp","projectId":"file-server-ab-git","tagName":"ab-v1"}),
            )?,
        ),
        (
            "git-delete-branch",
            json_spec(
                Method::POST,
                "/api/git/branch-delete",
                json!({"workspaceType":"pageApp","projectId":"file-server-ab-git","branchName":"ab-review"}),
            )?,
        ),
        ("git-log", git_log),
    ])
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

fn prepare_git_fixture(root: &Path) -> Result<()> {
    let project = root.join("project-workspace/file-server-ab-git");
    fs::create_dir_all(project.join("src"))?;
    fs::write(project.join("README.md"), "Git A/B fixture\n")?;
    fs::write(project.join("src/index.html"), "<main>Git fixture</main>\n")?;
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
    for (project_id, template_type) in [
        ("file-server-ab-build-react", "react"),
        ("file-server-ab-build-vue", "vue3"),
    ] {
        let create_case = format!("build-{template_type}-create-project");
        let create_spec = json_spec(
            Method::POST,
            "/api/project/create-project",
            json!({"projectId":project_id,"templateType":template_type}),
        )?;
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
                if rust_server.port != 4000 || !(4000..=55_000).contains(&ts_server.port) {
                    case.differences.push(assertion_difference(
                        &start_case,
                        "/assertions/dev-port",
                        format!(
                            "expected Rust port 4000 and TypeScript port in 4000-55000; Rust={}, TypeScript={}",
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
                        if rust_server.port != 4000 || !(4000..=55_000).contains(&ts_server.port) {
                            case.differences.push(assertion_difference(
                                &restart_case,
                                "/assertions/dev-port",
                                format!(
                                    "expected Rust port 4000 and TypeScript port in 4000-55000; Rust={}, TypeScript={}",
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

fn validate_git_log_response(body: &[u8]) -> Result<String, String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("git log response is not JSON: {error}"))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("git log response must contain success=true".into());
    }
    if value.get("total").and_then(Value::as_u64) != Some(1) {
        return Err("git log should return exactly the single fixture commit".into());
    }
    let commits = value
        .get("commits")
        .and_then(Value::as_array)
        .filter(|commits| commits.len() == 1)
        .ok_or_else(|| "git log should contain one commit entry".to_string())?;
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
    if commit.get("author_name").and_then(Value::as_str) != Some("File Server A-B")
        || commit.get("author_email").and_then(Value::as_str) != Some("ab@example.invalid")
        || commit
            .get("message")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err("git log entry is missing the fixture author or message".into());
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
    for file in ["react-vite-template.zip", "vue3-vite-template.zip"] {
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
}
