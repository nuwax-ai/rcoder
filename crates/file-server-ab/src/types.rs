//! A/B 对照共享数据结构、常量与 CLI 形状。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::report::append_jsonl;
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::{Method, StatusCode, header::HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const BODY_CAPTURE_LIMIT: usize = 2 * 1024 * 1024;

pub(crate) const HEALTH_TIMEOUT: Duration = Duration::from_secs(90);

pub(crate) const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) const CASE_USER: &str = "file-server-ab-user";

pub(crate) const CASE_CID: &str = "file-server-ab-session";

pub(crate) const AB_MULTIPART_BOUNDARY: &str = "----file-server-ab-boundary-6d86f5";

pub(crate) const AB_PACKAGE_CID: &str = "file-server-ab-package-session";

pub(crate) const AB_IMPORT_CID: &str = "file-server-ab-import-session";

pub(crate) const AB_SKILLS_V1_CID: &str = "file-server-ab-skills-v1-session";

pub(crate) const AB_SKILLS_V2_CID: &str = "file-server-ab-skills-v2-session";

pub(crate) const AB_LOG_CID: &str = "file-server-ab-log-session";

pub(crate) const ZIP_ENTRY_SIZE_LIMIT: u64 = 64 * 1024 * 1024;

pub(crate) const ZIP_TOTAL_SIZE_LIMIT: u64 = 128 * 1024 * 1024;

pub(crate) const ZIP_ENTRY_COUNT_LIMIT: usize = 20_000;

#[derive(Debug, Parser)]
#[command(
    name = "file-server-ab",
    about = "Black-box A/B comparison for file-server implementations"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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
pub(crate) enum Suite {
    Core,
    Git,
    Build,
    All,
}

impl Suite {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Git => "git",
            Self::Build => "build",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Manifest {
    pub(crate) run_id: String,
    pub(crate) suite: String,
    pub(crate) started_at: String,
    pub(crate) configuration_profile: String,
    pub(crate) rust_source: String,
    pub(crate) ts_source: String,
    pub(crate) rust_image: String,
    pub(crate) ts_image: String,
    pub(crate) docker_builder: String,
    pub(crate) rust_node_version: String,
    pub(crate) ts_node_version: String,
    pub(crate) rust_runtime_architecture: String,
    pub(crate) ts_runtime_architecture: String,
    pub(crate) rust_pnpm_version: String,
    pub(crate) ts_pnpm_version: String,
    pub(crate) pnpm_registry: String,
    pub(crate) pnpm_network_concurrency: String,
    pub(crate) pnpm_rust_store_volume: String,
    pub(crate) pnpm_rust_metadata_cache_volume: String,
    pub(crate) pnpm_typescript_store_volume: String,
    pub(crate) pnpm_typescript_metadata_cache_volume: String,
    pub(crate) rust_git_version: String,
    pub(crate) ts_git_version: String,
    pub(crate) fixtures: BTreeMap<String, String>,
    pub(crate) rules_sha256: String,
    pub(crate) route_coverage_sha256: String,
    pub(crate) route_coverage_baseline_matches: bool,
    pub(crate) endpoints: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RulesFile {
    pub(crate) schema_version: u32,
    pub(crate) rules: Vec<DifferenceRule>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DifferenceRule {
    pub(crate) case: String,
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) expected_rust: Value,
    pub(crate) expected_typescript: Value,
    pub(crate) reason: String,
    pub(crate) reviewed_by: String,
    pub(crate) expires_on: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct RequestLine {
    pub(crate) request_id: String,
    pub(crate) case: String,
    pub(crate) side: String,
    pub(crate) method: String,
    pub(crate) url: String,
    pub(crate) request_headers: BTreeMap<String, String>,
    pub(crate) request_body_bytes: usize,
    pub(crate) request_body_sha256: String,
    pub(crate) request_body_file: Option<String>,
    pub(crate) status: Option<u16>,
    pub(crate) response_headers: BTreeMap<String, String>,
    pub(crate) response_body_bytes: Option<usize>,
    pub(crate) response_body_sha256: Option<String>,
    pub(crate) response_body_file: Option<String>,
    pub(crate) request_body_truncated: bool,
    pub(crate) response_body_truncated: bool,
    pub(crate) headers_elapsed_ms: Option<u128>,
    pub(crate) elapsed_ms: u128,
    pub(crate) transport_error: Option<String>,
}

/// Written before a request is sent, so an interrupted run still shows requests
/// that were started but never produced a result line.
#[derive(Debug, Serialize)]
pub(crate) struct RequestStartLine {
    pub(crate) request_id: String,
    pub(crate) case: String,
    pub(crate) side: String,
    pub(crate) method: String,
    pub(crate) url: String,
    pub(crate) started_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct Difference {
    pub(crate) case: String,
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) rust_value: Option<Value>,
    pub(crate) ts_value: Option<Value>,
    pub(crate) accepted: bool,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct CaseResult {
    pub(crate) case: String,
    pub(crate) equal: bool,
    pub(crate) compared: String,
    pub(crate) normalized_paths: Vec<String>,
    pub(crate) normalized_headers: Vec<String>,
    pub(crate) rust_status: Option<u16>,
    pub(crate) ts_status: Option<u16>,
    pub(crate) differences: Vec<Difference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blocked_by: Option<String>,
}

/// Where a recorded request was sent. Only API requests can verify route coverage;
/// dev-server probes reuse paths such as `/` that belong to the API service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestTarget {
    Api,
    DevServer,
}

/// In-memory mirror of `requests.jsonl` used to cross-check route coverage against
/// the requests that were actually sent, including requests whose paired case never
/// completed because the run exited in between.
#[derive(Debug)]
pub(crate) struct RequestEvidence {
    pub(crate) case: String,
    pub(crate) side: String,
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) target: RequestTarget,
}

pub(crate) struct RequestJournal {
    pub(crate) report_dir: PathBuf,
    pub(crate) file: fs::File,
    pub(crate) evidence: Vec<RequestEvidence>,
    pub(crate) started: Vec<(String, String)>,
}

impl RequestJournal {
    pub(crate) fn new(report_dir: PathBuf) -> Result<Self> {
        let file = fs::File::create(report_dir.join("requests.jsonl"))
            .with_context(|| format!("create {}", report_dir.join("requests.jsonl").display()))?;
        Ok(Self {
            report_dir,
            file,
            evidence: Vec::new(),
            started: Vec::new(),
        })
    }

    pub(crate) fn begin(&mut self, start: &RequestStartLine) -> Result<()> {
        append_jsonl(&mut self.file, start)?;
        self.started.push((start.case.clone(), start.side.clone()));
        Ok(())
    }

    pub(crate) fn append(
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

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct SnapshotEntry {
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) sha256: Option<String>,
    pub(crate) size_bytes: Option<u64>,
    pub(crate) mode: Option<String>,
    pub(crate) symlink_target: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GitStateSnapshot {
    pub(crate) repository_exists: bool,
    pub(crate) head_reference: Option<String>,
    pub(crate) head_tree: Option<String>,
    pub(crate) refs: BTreeMap<String, String>,
    pub(crate) index_entries: Vec<String>,
    pub(crate) status_entries: Vec<String>,
    pub(crate) capture_errors: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct GitScenario {
    pub(crate) name: String,
    pub(crate) project_id: String,
    pub(crate) spec: RequestSpec,
    pub(crate) preparation: Option<GitPreparation>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum GitPreparation {
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

pub(crate) const GIT_PROJECT_MAIN: &str = "file-server-ab-git";

pub(crate) const GIT_PROJECT_BRANCH_DELETE: &str = "file-server-ab-git-branch-delete";

pub(crate) const GIT_PROJECT_REVERT: &str = "file-server-ab-git-revert";

pub(crate) const GIT_PROJECT_RESET_MIXED: &str = "file-server-ab-git-reset-mixed";

pub(crate) const GIT_PROJECT_RESET_HARD: &str = "file-server-ab-git-reset-hard";

pub(crate) const GIT_PROJECT_RESET_SOFT: &str = "file-server-ab-git-reset-soft";

pub(crate) const GIT_PROJECT_CHECKOUT: &str = "file-server-ab-git-checkout";

pub(crate) const GIT_PROJECT_DISCARD: &str = "file-server-ab-git-discard";

pub(crate) const GIT_PROJECT_MERGE_CONFLICT: &str = "file-server-ab-git-merge-conflict";

pub(crate) const GIT_PROJECT_IDS: &[&str] = &[
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
pub(crate) struct RunDiff {
    pub(crate) cases: Vec<CaseResult>,
    pub(crate) initial_state_differences: Vec<Difference>,
    pub(crate) state_differences: Vec<Difference>,
    pub(crate) git_state_differences: Vec<Difference>,
    pub(crate) summary: Summary,
}

#[derive(Debug, Serialize, Clone)]
pub(crate) struct Summary {
    pub(crate) comparisons: usize,
    pub(crate) equal: usize,
    pub(crate) expected_differences: usize,
    pub(crate) unclassified_differences: usize,
    pub(crate) transport_errors: usize,
    pub(crate) environment_errors: usize,
    pub(crate) environment_blocked: bool,
    pub(crate) precondition_errors: usize,
    pub(crate) blocked_cases: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) incomplete_reason: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DifferenceGroup {
    pub(crate) scope: String,
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) accepted: bool,
    pub(crate) occurrences: usize,
    pub(crate) cases: BTreeSet<String>,
}

pub(crate) type DifferenceGroupKey = (String, String, String, bool);

pub(crate) type DifferenceGroupCounts = (usize, BTreeSet<String>);

#[derive(Debug)]
pub(crate) struct Exchange {
    pub(crate) status: Option<StatusCode>,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
    pub(crate) transport_error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct RequestSpec {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) body: Vec<u8>,
    pub(crate) content_type: Option<&'static str>,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) health_probe: bool,
    pub(crate) expected_status: ExpectedStatus,
    pub(crate) normalized_paths: Vec<String>,
    pub(crate) normalized_headers: Vec<String>,
    pub(crate) timeout: Duration,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ExpectedStatus {
    Success2xx,
    ClientError4xx,
    PartialContent206,
    NotModified304,
}

impl ExpectedStatus {
    pub(crate) fn matches(self, status: Option<StatusCode>) -> bool {
        status.is_some_and(|status| match self {
            Self::Success2xx => status.is_success(),
            Self::ClientError4xx => status.is_client_error(),
            Self::PartialContent206 => status == StatusCode::PARTIAL_CONTENT,
            Self::NotModified304 => status == StatusCode::NOT_MODIFIED,
        })
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Success2xx => "2xx",
            Self::ClientError4xx => "4xx",
            Self::PartialContent206 => "206",
            Self::NotModified304 => "304",
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct RunOptions {
    #[arg(long, value_enum, default_value_t = Suite::Core)]
    pub(crate) suite: Suite,
    #[arg(long)]
    pub(crate) run_id: Option<String>,
    #[arg(long)]
    pub(crate) rules: PathBuf,
    #[arg(long)]
    pub(crate) route_coverage: PathBuf,
    #[arg(long)]
    pub(crate) rust_url: String,
    #[arg(long)]
    pub(crate) ts_url: String,
    #[arg(long, default_value = "")]
    pub(crate) rust_dev_url: String,
    #[arg(long, default_value = "")]
    pub(crate) ts_dev_url: String,
    #[arg(long)]
    pub(crate) rust_root: PathBuf,
    #[arg(long)]
    pub(crate) ts_root: PathBuf,
    #[arg(long)]
    pub(crate) fixtures: PathBuf,
    #[arg(long)]
    pub(crate) report_root: PathBuf,
}
