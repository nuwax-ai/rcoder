//! build/dev 生命周期套件（阶段状态机 + run_build_suite）。

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use reqwest::{Client, Method};
use serde_json::{Value, json};

use crate::assertions::*;
use crate::http::*;
use crate::manifest::*;
use crate::orchestrate::*;
use crate::scenarios::*;
use crate::types::*;

#[derive(Debug)]
pub(crate) struct DevServerIdentity {
    pub(crate) pid: u64,
    pub(crate) port: u16,
}

pub(crate) fn react_log_stages() -> Vec<String> {
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

pub(crate) fn lifecycle_tail(template_type: &str) -> Vec<String> {
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

pub(crate) fn stages_after_build(template_type: &str) -> Vec<String> {
    let mut stages = vec![
        format!("build-{template_type}-static-dist-index"),
        format!("build-{template_type}-start-dev"),
    ];
    stages.extend(stages_after_start(template_type));
    stages
}

pub(crate) fn stages_after_start(template_type: &str) -> Vec<String> {
    let mut stages = vec![format!("build-{template_type}-dev-http-reachable")];
    if template_type == "react" {
        stages.extend(react_log_stages());
    }
    stages.extend(lifecycle_tail(template_type));
    stages
}

pub(crate) fn stages_after_probe(template_type: &str) -> Vec<String> {
    stages_after_start(template_type)
}

pub(crate) fn stages_after_log_stats() -> Vec<String> {
    vec![
        "build-react-clear-log-cache".to_string(),
        "build-react-log-cache-stats-after-clear".to_string(),
    ]
}

pub(crate) fn stages_after_restart(template_type: &str) -> Vec<String> {
    [
        format!("build-{template_type}-restarted-dev-http-reachable"),
        format!("build-{template_type}-stop-dev"),
        format!("build-{template_type}-stop-dev-port-unreachable"),
        format!("build-{template_type}-list-after-stop"),
    ]
    .to_vec()
}

pub(crate) fn stages_after_stop(template_type: &str) -> Vec<String> {
    stages_after_restart(template_type)
}

/// Record the given stage names as blocked, in dependency order. Stages that were
/// already recorded (executed or blocked) are skipped, so chains can be listed
/// redundantly after partial progress.
pub(crate) fn record_blocked_stages(
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
pub(crate) async fn run_gated_pair(
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
pub(crate) async fn stop_dev_cleanup(
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

pub(crate) async fn run_build_suite(
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
