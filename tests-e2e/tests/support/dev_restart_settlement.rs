//! Read the exact admitted compute operations before destructive fixture cleanup.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;

// Preserve the scope fixture's original restart-observation budget. All
// distinct operations in one observation share this deadline.
pub const BUDGET: Duration = Duration::from_secs(150);

pub async fn wait(
    client: &Client,
    base: &str,
    app: &str,
    admitted: &[Value],
    deadline: Instant,
) -> Result<Vec<Value>, String> {
    let mut pending = BTreeMap::new();
    for envelope in admitted {
        let record = &envelope["data"];
        let operation = record["operation_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| format!("accepted restart has no original operation ID: {envelope}"))?;
        let lifecycle = record["lifecycle_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| format!("accepted restart has no lifecycle: {envelope}"))?;
        if envelope["code"] != "0000"
            || envelope["success"] != true
            || record["app_id"] != app
            || record["scope"] != "Dev"
            || record["action"] != "restart"
        {
            return Err(format!(
                "accepted restart identity is inconsistent: {envelope}"
            ));
        }
        if let Some(previous) = pending.insert(operation.to_owned(), lifecycle.to_owned())
            && previous != lifecycle
        {
            return Err("same restart operation was returned for different lifecycles".into());
        }
    }
    if pending.is_empty() {
        return Err("no admitted restart operation was captured".into());
    }
    let mut completed = Vec::new();
    while !pending.is_empty() {
        let operations: Vec<_> = pending.keys().cloned().collect();
        for operation in operations {
            if Instant::now() >= deadline {
                return Err(format!(
                    "original restart settlement deadline expired: {pending:?}"
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let path = format!("/computer/pod/operations/{app}/{operation}");
            let (status, body) = tokio::time::timeout_at(deadline.into(), async {
                let response = client
                    .get(format!("{base}{path}"))
                    .timeout(Duration::from_secs(60).min(remaining))
                    .send()
                    .await
                    .map_err(|error| format!("{path}: {error}"))?;
                let status = response.status();
                let body = response
                    .json::<Value>()
                    .await
                    .map_err(|error| format!("{path}: decode response: {error}"))?;
                Ok::<_, String>((status, body))
            })
            .await
            .map_err(|_| {
                format!("original restart query exceeded shared deadline: {operation}")
            })??;
            if Instant::now() >= deadline {
                return Err(format!(
                    "original restart reply arrived after deadline: {operation}"
                ));
            }
            let record = &body["data"];
            if !status.is_success()
                || body["code"] != "0000"
                || body["success"] != true
                || record["app_id"] != app
                || record["operation_id"] != operation
                || record["lifecycle_id"].as_str() != pending.get(&operation).map(String::as_str)
                || record["scope"] != "Dev"
                || record["action"] != "restart"
            {
                return Err(format!(
                    "original restart query changed or lost identity: HTTP {status}: {body}"
                ));
            }
            match record["state"].as_str() {
                Some("succeeded") if record["stage"] == "completed" => {
                    pending.remove(&operation);
                    completed.push(record.clone());
                }
                Some("pending" | "running") => {}
                Some("failed" | "superseded" | "recovery_required" | "succeeded") | None => {
                    return Err(format!(
                        "original restart did not complete successfully: {record}"
                    ));
                }
                Some(state) => {
                    return Err(format!("unknown original restart state {state}: {record}"));
                }
            }
        }
        if !pending.is_empty() {
            tokio::time::sleep(
                Duration::from_secs(1).min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }
    Ok(completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::OriginalUri, routing::get};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn record(operation: &str, state: &str, stage: &str) -> Value {
        json!({"success":true,"code":"0000","data":{
            "app_id":"owned-app","lifecycle_id":"original-life","operation_id":operation,
            "scope":"Dev","action":"restart","state":state,"stage":stage}})
    }

    struct Server {
        base: String,
        paths: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn server(replies: Vec<Value>, delay: Duration) -> Server {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let observed = paths.clone();
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let router = Router::new().route(
            "/{*path}",
            get(move |OriginalUri(uri): OriginalUri| {
                let paths = observed.clone();
                let replies = replies.clone();
                async move {
                    paths.lock().unwrap().push(uri.path().to_owned());
                    tokio::time::sleep(delay).await;
                    if uri.path().ends_with("/operations/current") {
                        return Json(json!({"success":true,"code":"0000","data":[]}));
                    }
                    Json(
                        replies
                            .lock()
                            .unwrap()
                            .pop_front()
                            .expect("scripted exact operation query"),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Server { base, paths, task }
    }

    #[tokio::test]
    async fn duplicate_admissions_wait_for_the_exact_original_success_and_never_current_absence() {
        let running = record("original-op", "running", "starting");
        let succeeded = record("original-op", "succeeded", "completed");
        let server = server(vec![running.clone(), succeeded.clone()], Duration::ZERO).await;
        let result = wait(
            &Client::new(),
            &server.base,
            "owned-app",
            &[running.clone(), running],
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(result, vec![succeeded["data"].clone()]);
        assert_eq!(
            *server.paths.lock().unwrap(),
            vec![
                "/computer/pod/operations/owned-app/original-op",
                "/computer/pod/operations/owned-app/original-op"
            ]
        );
    }

    #[tokio::test]
    async fn wrong_identity_and_non_success_outcomes_cannot_authorize_fixture_purge() {
        let accepted = record("original-op", "pending", "admitted");
        let mut replies = Vec::new();
        for field in ["app_id", "operation_id", "lifecycle_id", "scope", "action"] {
            let mut wrong = record("original-op", "succeeded", "completed");
            wrong["data"][field] = "different".into();
            replies.push(wrong);
        }
        for state in ["failed", "superseded", "recovery_required", "unknown"] {
            replies.push(record("original-op", state, "starting"));
        }
        replies.push(record("original-op", "succeeded", "starting"));
        replies.push(json!({"success":true,"code":"0000","data":[]}));
        for reply in replies {
            let server = server(vec![reply], Duration::ZERO).await;
            assert!(
                wait(
                    &Client::new(),
                    &server.base,
                    "owned-app",
                    std::slice::from_ref(&accepted),
                    Instant::now() + Duration::from_secs(5)
                )
                .await
                .is_err()
            );
            assert_eq!(server.paths.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn distinct_original_operations_share_one_deadline() {
        let first = record("first", "succeeded", "completed");
        let second = record("second", "succeeded", "completed");
        let server = server(
            vec![first.clone(), second.clone()],
            Duration::from_millis(200),
        )
        .await;
        assert!(
            wait(
                &Client::new(),
                &server.base,
                "owned-app",
                &[first, second],
                Instant::now() + Duration::from_millis(300)
            )
            .await
            .is_err()
        );
    }
}
