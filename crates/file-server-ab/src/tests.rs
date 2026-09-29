//! 单元测试（对比引擎/清单/依赖阻断/规则/zip/快照/头协议）。

use crate::assertions::*;
use crate::compare::*;
use crate::http::*;
use crate::manifest::*;
use crate::orchestrate::*;
use crate::report::*;
use crate::scenarios::*;

use crate::scenarios::lifecycle::*;
use crate::snapshot::*;
use crate::types::*;

use serde_json::{Value, json};
use std::fs;
use std::io::Cursor;

use std::io::Write as IoWrite;

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
    let mut recorder =
        Recorder::new(report_dir.clone(), Suite::Core, json!({"entries": []})).expect("recorder");
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
    let mut recorder =
        Recorder::new(report_dir.clone(), Suite::Core, json!({"entries": []})).expect("recorder");

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
    let paths = |seen: &std::sync::Arc<std::sync::Mutex<Vec<String>>>| seen.lock().unwrap().clone();
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
