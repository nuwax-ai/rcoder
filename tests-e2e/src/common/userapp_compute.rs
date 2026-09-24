//! One UserApp dev compute contract shared by Compose, host Docker and host K8s.
//! The probe owns physical evidence; this module owns the HTTP operation order.

use super::{Env, report::JsonlReporter};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[async_trait]
pub trait DevComputeProbe {
    async fn prepare(&mut self, app_id: &str, marker: &str) -> Result<(), String>;
    async fn stopped(&mut self, app_id: &str) -> Result<(), String>;
    async fn restarted(&mut self, app_id: &str, marker: &str) -> Result<(), String>;
}

async fn request(
    env: &Env,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<(StatusCode, Value), String> {
    let builder = env
        .http
        .request(method, format!("{}{path}", env.rcoder))
        .timeout(Duration::from_secs(300));
    let response = match body {
        Some(value) => builder.json(&value),
        None => builder,
    }
    .send()
    .await
    .map_err(|error| format!("{path}: {error}"))?;
    let status = response.status();
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| format!("{path}: decode response: {error}"))?;
    Ok((status, value))
}

fn success(status: StatusCode, value: &Value) -> bool {
    status.is_success() && value["code"].as_str() == Some("0000")
}

async fn lifecycle(env: &Env, app_id: &str) -> Result<String, String> {
    let (status, body) = request(
        env,
        reqwest::Method::GET,
        &format!("/api/v1/userapp/{app_id}/lifecycle"),
        None,
    )
    .await?;
    if !success(status, &body) {
        return Err(format!("lifecycle HTTP {status}: {body}"));
    }
    body["data"]["lifecycle_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("lifecycle_id missing: {body}"))
}

async fn control(env: &Env, app_id: &str, lifecycle_id: &str, action: &str) -> Result<(), String> {
    let (status, body) = request(
        env,
        reqwest::Method::POST,
        &format!("/computer/pod/{action}"),
        Some(json!({
            "app_id": app_id,
            "app_stage": "dev",
            "service_type": "userapp",
            "lifecycle_id": lifecycle_id,
            "request_id": format!("e2e{action}{app_id}"),
        })),
    )
    .await?;
    if !success(status, &body) {
        return Err(format!("{action} HTTP {status}: {body}"));
    }
    // Older Docker builders removed by AutoRemove have a synchronous restart
    // recreation path. The physical probe still has to prove it succeeded.
    if action == "restart" && status == StatusCode::OK && body["data"]["restarted"] == true {
        return Ok(());
    }
    if status != StatusCode::ACCEPTED {
        return Err(format!(
            "{action}: expected 202 operation, got HTTP {status}: {body}"
        ));
    }
    let operation_id = body["data"]["operation_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| format!("{action}: operation_id missing: {body}"))?;
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let (query_status, query) = request(
            env,
            reqwest::Method::GET,
            &format!("/computer/pod/operations/{app_id}/{operation_id}"),
            None,
        )
        .await?;
        if !success(query_status, &query) {
            return Err(format!(
                "{action} operation query HTTP {query_status}: {query}"
            ));
        }
        let record = &query["data"];
        if record["lifecycle_id"].as_str() != Some(lifecycle_id)
            || record["operation_id"].as_str() != Some(operation_id)
            || record["action"].as_str() != Some(action)
        {
            return Err(format!("{action} operation identity changed: {record}"));
        }
        match record["state"].as_str() {
            Some("succeeded") if record["stage"] == "completed" => return Ok(()),
            Some("failed" | "recovery_required" | "superseded") => {
                return Err(format!("{action} operation did not succeed: {record}"));
            }
            Some("pending" | "running") if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            _ => {
                return Err(format!(
                    "{action} operation timed out or has unknown state: {record}"
                ));
            }
        }
    }
}

/// Caller cleans up its owned runtime after this returns, including on failure.
pub async fn run_dev_compute_cycle<P: DevComputeProbe + Send>(
    env: &Env,
    report: &JsonlReporter,
    app_id: &str,
    probe: &mut P,
    register_docker: bool,
) -> Option<String> {
    let workspace = request(
        env,
        reqwest::Method::POST,
        "/api/v1/userapp/workspace",
        Some(json!({"app_id": app_id})),
    )
    .await;
    if register_docker {
        let required = workspace
            .as_ref()
            .is_ok_and(|(status, body)| success(*status, body));
        let receipt = super::resources::register_builder_attempt(app_id, required);
        report.assert_hard(
            "userapp builder ownership recorded",
            receipt.is_ok(),
            format!("{receipt:?}"),
        );
        if receipt.is_err() {
            return None;
        }
    }
    let created = workspace.and_then(|(status, body)| {
        if success(status, &body) && body["data"]["container_name"].as_str().is_some() {
            Ok(())
        } else {
            Err(format!("workspace HTTP {status}: {body}"))
        }
    });
    report.assert_hard(
        "userapp workspace created",
        created.is_ok(),
        format!("{created:?}"),
    );
    if created.is_err() {
        return None;
    }
    let identity = lifecycle(env, app_id).await;
    report.assert_hard(
        "userapp lifecycle identified",
        identity.is_ok(),
        format!("{identity:?}"),
    );
    let Ok(lifecycle_id) = identity else {
        return None;
    };
    let marker = format!("e2e-{app_id}");
    let prepared = probe.prepare(app_id, &marker).await;
    report.assert_hard(
        "userapp physical workspace marked",
        prepared.is_ok(),
        format!("{prepared:?}"),
    );
    if prepared.is_err() {
        return Some(lifecycle_id);
    }

    let stopped = control(env, app_id, &lifecycle_id, "stop").await;
    report.assert_hard(
        "userapp dev stop completed",
        stopped.is_ok(),
        format!("{stopped:?}"),
    );
    if stopped.is_err() {
        return Some(lifecycle_id);
    }
    let stopped_probe = probe.stopped(app_id).await;
    report.assert_hard(
        "userapp compute stopped with storage retained",
        stopped_probe.is_ok(),
        format!("{stopped_probe:?}"),
    );
    if stopped_probe.is_err() {
        return Some(lifecycle_id);
    }

    let restarted = control(env, app_id, &lifecycle_id, "restart").await;
    report.assert_hard(
        "userapp dev restart completed",
        restarted.is_ok(),
        format!("{restarted:?}"),
    );
    if restarted.is_err() {
        return Some(lifecycle_id);
    }
    let resumed = probe.restarted(app_id, &marker).await;
    report.assert_hard(
        "userapp workspace and address survived restart",
        resumed.is_ok(),
        format!("{resumed:?}"),
    );
    let current = lifecycle(env, app_id).await;
    report.assert_hard(
        "userapp lifecycle unchanged",
        current.as_deref() == Ok(lifecycle_id.as_str()),
        format!("before={lifecycle_id}, after={current:?}"),
    );
    Some(lifecycle_id)
}

pub async fn purge_owned_app(
    env: &Env,
    app_id: &str,
    lifecycle_id: Option<&str>,
) -> Result<(), String> {
    let lifecycle_id = lifecycle_id.ok_or("test application lifecycle was not confirmed")?;
    let (status, body) = request(
        env,
        reqwest::Method::POST,
        &format!("/api/v1/userapp/{app_id}/delete/app"),
        Some(json!({"lifecycle_id": lifecycle_id, "request_id": format!("e2epurge{app_id}")})),
    )
    .await?;
    if success(status, &body) {
        Ok(())
    } else {
        Err(format!("purge HTTP {status}: {body}"))
    }
}
