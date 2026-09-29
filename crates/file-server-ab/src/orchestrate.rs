//! 套件编排：Recorder 落账 + run_suite 主流程（健康探测→场景→快照→汇总）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::assertions::*;
use crate::compare::*;
use crate::http::*;
use crate::manifest::*;
use crate::report::*;
use crate::scenarios::core::*;
use crate::scenarios::git::*;
use crate::scenarios::lifecycle::*;
use crate::scenarios::*;
use crate::snapshot::*;
use crate::types::*;
use crate::util::*;

/// Accumulates completed cases and rewrites the route-coverage evidence after every
/// case, so an interrupted run still leaves an accurate partial report on disk.
pub(crate) struct Recorder {
    pub(crate) report_dir: PathBuf,
    pub(crate) suite: Suite,
    pub(crate) route_coverage: Value,
    pub(crate) cases: Vec<CaseResult>,
    pub(crate) cases_file: fs::File,
}

impl Recorder {
    pub(crate) fn new(report_dir: PathBuf, suite: Suite, route_coverage: Value) -> Result<Self> {
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

    pub(crate) fn record(&mut self, journal: &RequestJournal, case: CaseResult) -> Result<()> {
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
    pub(crate) fn write_incomplete(
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

pub(crate) async fn run_suite(options: RunOptions) -> Result<()> {
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
