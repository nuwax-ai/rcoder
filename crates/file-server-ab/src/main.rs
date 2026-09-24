use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
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
    Run {
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        rules: PathBuf,
        #[arg(long)]
        rust_url: String,
        #[arg(long)]
        ts_url: String,
        #[arg(long)]
        rust_root: PathBuf,
        #[arg(long)]
        ts_root: PathBuf,
        #[arg(long)]
        fixtures: PathBuf,
        #[arg(long)]
        report_root: PathBuf,
    },
}

#[derive(Debug, Serialize)]
struct Manifest {
    run_id: String,
    started_at: String,
    configuration_profile: String,
    rust_source: String,
    ts_source: String,
    rust_image: String,
    ts_image: String,
    node_version: String,
    runtime_architecture: String,
    pnpm_version: String,
    git_version: String,
    fixtures: BTreeMap<String, String>,
    rules_sha256: String,
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
    health_probe: bool,
    expected_status: ExpectedStatus,
    normalized_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum ExpectedStatus {
    Success2xx,
    ClientError4xx,
}

impl ExpectedStatus {
    fn matches(self, status: Option<StatusCode>) -> bool {
        status.is_some_and(|status| match self {
            Self::Success2xx => status.is_success(),
            Self::ClientError4xx => status.is_client_error(),
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::Success2xx => "2xx",
            Self::ClientError4xx => "4xx",
        }
    }
}

struct RunOptions {
    run_id: Option<String>,
    rules: PathBuf,
    rust_url: String,
    ts_url: String,
    rust_root: PathBuf,
    ts_root: PathBuf,
    fixtures: PathBuf,
    report_root: PathBuf,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Doctor { rust_url, ts_url } => doctor(&rust_url, &ts_url).await,
        Command::Run {
            run_id,
            rules,
            rust_url,
            ts_url,
            rust_root,
            ts_root,
            fixtures,
            report_root,
        } => {
            run_suite(RunOptions {
                run_id,
                rules,
                rust_url,
                ts_url,
                rust_root,
                ts_root,
                fixtures,
                report_root,
            })
            .await
        }
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
        run_id: requested_run_id,
        rules: rules_path,
        rust_url,
        ts_url,
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

    let fixture_hashes = hash_fixtures(&fixtures)?;
    let rules_bytes = fs::read(&rules_path)
        .with_context(|| format!("read A/B difference rules {}", rules_path.display()))?;
    let rules: RulesFile = serde_json::from_slice(&rules_bytes)
        .with_context(|| format!("parse A/B difference rules {}", rules_path.display()))?;
    validate_rules(&rules)?;
    let mut endpoints = BTreeMap::new();
    endpoints.insert("rust".to_string(), rust_url.clone());
    endpoints.insert("typescript".to_string(), ts_url.clone());
    let manifest = Manifest {
        run_id: run_id.clone(),
        started_at: timestamp_rfc3339(),
        configuration_profile: "docker-compose-main-container".to_string(),
        rust_source: env_or("AB_RUST_SOURCE", "unknown"),
        ts_source: env_or("AB_TS_SOURCE", "unknown"),
        rust_image: env_or("AB_RUST_IMAGE", "unknown"),
        ts_image: env_or("AB_TS_IMAGE", "unknown"),
        node_version: env_or("AB_NODE_VERSION", "unknown"),
        runtime_architecture: env_or("AB_NODE_ARCH", "unknown"),
        pnpm_version: env_or("AB_PNPM_VERSION", "unknown"),
        git_version: env_or("AB_GIT_VERSION", "unknown"),
        fixtures: fixture_hashes,
        rules_sha256: sha256(&rules_bytes),
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
    for spec in core_scenarios()? {
        let case = run_pair(
            &client,
            &report_dir,
            &mut requests,
            &rust_url,
            &ts_url,
            spec,
        )
        .await?;
        println!("{} {}", if case.equal { "PASS" } else { "DIFF" }, case.case);
        cases.push(case);
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
            health_probe: false,
            expected_status: ExpectedStatus::Success2xx,
            normalized_paths: Vec::new(),
        })
    };
    let get = |path: String| RequestSpec {
        method: Method::GET,
        path,
        body: Vec::new(),
        content_type: None,
        health_probe: false,
        expected_status: ExpectedStatus::Success2xx,
        normalized_paths: Vec::new(),
    };
    let get_client_error = |path: String| RequestSpec {
        expected_status: ExpectedStatus::ClientError4xx,
        normalized_paths: vec!["/error/requestId".into(), "/error/timestamp".into()],
        ..get(path)
    };
    let user = CASE_USER;
    let cid = CASE_CID;
    let root = "/data/computer-workspace/file-server-ab-user/file-server-ab-session";
    Ok(vec![
        (
            "health",
            RequestSpec {
                method: Method::GET,
                path: "/health".to_string(),
                body: Vec::new(),
                content_type: None,
                health_probe: true,
                expected_status: ExpectedStatus::Success2xx,
                normalized_paths: Vec::new(),
            },
        ),
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
            get("/api/page/static/file-server-ab-react/src/file-server-ab.txt".to_string()),
        ),
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

async fn run_pair(
    client: &Client,
    report_dir: &Path,
    requests: &mut fs::File,
    rust_url: &str,
    ts_url: &str,
    (case, spec): (&'static str, RequestSpec),
) -> Result<CaseResult> {
    let ts = exchange(
        client,
        report_dir,
        requests,
        case,
        "typescript",
        ts_url,
        &spec,
    )
    .await?;
    let rust = exchange(client, report_dir, requests, case, "rust", rust_url, &spec).await?;
    let differences = compare_exchange(
        case,
        &rust,
        &ts,
        spec.health_probe,
        spec.expected_status,
        &spec.normalized_paths,
    );
    let equal = differences.is_empty();
    Ok(CaseResult {
        case: case.to_string(),
        equal,
        compared: if spec.health_probe {
            "expected HTTP class, HTTP status and JSON status field; runtime metadata is not gated"
                .into()
        } else {
            "expected HTTP class, HTTP status, selected stable headers, and complete response body"
                .into()
        },
        normalized_paths: spec.normalized_paths,
        rust_status: rust.status.map(|status| status.as_u16()),
        ts_status: ts.status.map(|status| status.as_u16()),
        differences,
    })
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
    let response = request.timeout(Duration::from_secs(30)).send().await;
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
        if !case.normalized_paths.is_empty() {
            any_normalizations = true;
            writeln!(
                summary,
                "- `{}`: {}",
                case.case,
                case.normalized_paths
                    .iter()
                    .map(|path| format!("`{path}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
    }
    if !any_normalizations {
        writeln!(summary, "No JSON fields normalized.")?;
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
}
