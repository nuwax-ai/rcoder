//! Only the gate's read-only file query may retry a failed HTTP send.
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use tokio::{sync::watch, time::Instant};

use crate::common::{Env, report::JsonlReporter};

pub(super) struct GateFile<'a> {
    pub(super) entry: &'a str,
    pub(super) app: &'a str,
    pub(super) user: &'a str,
    pub(super) project: &'a str,
    pub(super) file: &'a str,
}

#[derive(Debug)]
struct ReadFailure {
    error: anyhow::Error,
    stage: &'static str,
    flags: Value,
    retry_send: bool,
}

impl ReadFailure {
    fn request(error: reqwest::Error, stage: &'static str) -> Self {
        let retry_send = stage == "send"
            && !error.is_builder()
            && !error.is_redirect()
            && !error.is_status()
            && !error.is_body()
            && !error.is_decode()
            && (error.is_timeout() || error.is_connect() || error.is_request());
        let flags = json!({
            "is_timeout":error.is_timeout(), "is_connect":error.is_connect(),
            "is_request":error.is_request(), "is_body":error.is_body(),
            "is_decode":error.is_decode(), "is_builder":error.is_builder(),
            "is_redirect":error.is_redirect(), "is_status":error.is_status(),
        });
        Self {
            error: error.into(),
            stage,
            flags,
            retry_send,
        }
    }

    fn response(error: anyhow::Error, stage: &'static str) -> Self {
        Self {
            error,
            stage,
            flags: Value::Null,
            retry_send: false,
        }
    }
}

async fn read_once(
    client: &reqwest::Client,
    file: &GateFile<'_>,
    timeout: Duration,
) -> std::result::Result<String, ReadFailure> {
    let response = client
        .post(format!("{}/api/git/file-content", file.entry))
        .header("X-Service-Type", "userapp")
        .header("X-App-Id", file.app)
        .json(
            &json!({"workspaceType":"userApp","serviceType":"userapp","appId":file.app,
            "userId":file.user,"cId":file.project,"projectId":file.project,
            "ref":"worktree","filePath":file.file}),
        )
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| ReadFailure::request(error, "send"))?;
    if !response.status().is_success() {
        return Err(ReadFailure::response(
            anyhow!("read {}: HTTP {}", file.file, response.status()),
            "http_status",
        ));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|error| ReadFailure::request(error, "json_body"))?;
    if value["success"] != true {
        return Err(ReadFailure::response(
            anyhow!("read {}: {}", file.file, value["message"]),
            "validation",
        ));
    }
    value["content"].as_str().map(str::to_owned).ok_or_else(|| {
        ReadFailure::response(anyhow!("missing content for {}", file.file), "validation")
    })
}

pub(super) async fn read_file(
    env: &Env,
    entry: &str,
    app: &str,
    user: &str,
    project: &str,
    file: &str,
) -> Result<String> {
    read_once(
        &env.http,
        &GateFile {
            entry,
            app,
            user,
            project,
            file,
        },
        Duration::from_secs(10),
    )
    .await
    .map_err(|failure| failure.error)
}

struct PollBudget {
    total: Duration,
    request: Duration,
    interval: Duration,
}

pub(super) async fn wait_for_gate(
    env: &Env,
    report: &JsonlReporter,
    file: GateFile<'_>,
    receiver: &mut watch::Receiver<bool>,
) -> Result<()> {
    poll_gate(
        &env.http,
        report,
        file,
        receiver,
        PollBudget {
            total: Duration::from_secs(90),
            request: Duration::from_secs(10),
            interval: Duration::from_millis(500),
        },
    )
    .await
}

async fn poll_gate(
    client: &reqwest::Client,
    report: &JsonlReporter,
    file: GateFile<'_>,
    receiver: &mut watch::Receiver<bool>,
    budget: PollBudget,
) -> Result<()> {
    let started = Instant::now();
    let deadline = started + budget.total;
    let mut attempts = 0_u32;
    let mut transport_retries = 0_u32;
    let mut transport_failures = 0_u32;
    let mut retry_transport = false;
    let mut last_transport = None;
    let outcome = tokio::time::timeout_at(deadline, async {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Let the enclosing deadline report the last cause. Do not
                // issue a zero-duration request or reset the total budget.
                return std::future::pending().await;
            }
            if retry_transport {
                transport_retries += 1;
                retry_transport = false;
            }
            attempts += 1;
            let request_timeout = budget.request.min(remaining);
            let request_started = Instant::now();
            let read = read_once(client, &file, request_timeout).await;
            let mut detail = json!({
                "file":file.file, "attempt":attempts,
                "request_elapsed_ms":request_started.elapsed().as_millis(),
                "poll_elapsed_ms":started.elapsed().as_millis(),
                "request_timeout_ms":request_timeout.as_millis(),
                "transport_retry_count":transport_retries,
                "transport_failure_count":transport_failures,
            });
            match read {
                Ok(content) => {
                    detail["gate_started"] = json!(content == "started");
                    detail["stream_started"] = json!(*receiver.borrow());
                    report.diagnostic("gate file-content read", "observed", &detail.to_string());
                    if content == "started" && *receiver.borrow() {
                        return Ok(());
                    }
                }
                Err(failure) => {
                    detail["stage"] = json!(failure.stage);
                    detail["reqwest_flags"] = failure.flags;
                    detail["retryable_read_transport"] = json!(failure.retry_send);
                    detail["error_chain"] = json!(format!("{:#}", failure.error));
                    report.diagnostic("gate file-content read", "failed", &detail.to_string());
                    if !failure.retry_send {
                        return Err(failure.error);
                    }
                    transport_failures += 1;
                    retry_transport = true;
                    last_transport = Some(failure.error);
                }
            }
            ensure!(
                receiver.has_changed().is_ok(),
                "agent stream ended before executing {}",
                file.file
            );
            tokio::time::sleep_until((Instant::now() + budget.interval).min(deadline)).await;
        }
    })
    .await;
    match outcome {
        Ok(result) => result,
        Err(_) => {
            let cause = last_transport.as_ref().map(|error| format!("{error:#}"));
            report.diagnostic(
                "gate file-content deadline",
                "expired",
                &json!({"file":file.file,"attempts":attempts,
                    "transport_retry_count":transport_retries,
                    "transport_failure_count":transport_failures,
                    "elapsed_ms":started.elapsed().as_millis(),"last_transport_error_chain":cause.as_deref()})
                .to_string(),
            );
            Err(anyhow!(
                "gate {} readiness deadline after {}ms; attempts={attempts}, transport_retries={transport_retries}; last transport cause: {}",
                file.file,
                started.elapsed().as_millis(),
                cause.as_deref().unwrap_or("none")
            ))
        }
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
