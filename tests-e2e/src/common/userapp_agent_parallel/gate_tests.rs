use std::{collections::VecDeque, sync::Arc};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, oneshot},
    task::JoinSet,
};

use super::*;

const STARTED: &str = r#"{"success":true,"content":"started"}"#;

#[derive(Clone, Copy)]
enum Response {
    Disconnect,
    Reset,
    Stall,
    Json(u16, &'static str),
}

struct Server {
    entry: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

async fn request_bytes(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 2048];
    loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "fixture client closed before request body");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                return bytes;
            }
        }
    }
}

impl Server {
    async fn new(replies: Vec<Response>, fallback: Response) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let entry = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut responses = VecDeque::from(replies);
            let mut children = JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        let response = responses.pop_front().unwrap_or(fallback);
                        let captured = Arc::clone(&captured);
                        children.spawn(async move {
                            let request = request_bytes(&mut stream).await;
                            captured.lock().await.push(request);
                            match response {
                                Response::Disconnect => {}
                                Response::Reset => stream.set_zero_linger().unwrap(),
                                Response::Stall => std::future::pending::<()>().await,
                                Response::Json(code, body) => {
                                    let reply = format!("HTTP/1.1 {code} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                                    stream.write_all(reply.as_bytes()).await.unwrap();
                                }
                            }
                        });
                    }
                    result = children.join_next(), if !children.is_empty() => {
                        result.unwrap().unwrap();
                    }
                }
            }
            children.abort_all();
            while let Some(result) = children.join_next().await {
                if let Err(error) = result {
                    assert!(error.is_cancelled());
                }
            }
        });
        Self {
            entry,
            requests,
            stop,
            task,
        }
    }

    async fn finish(self) -> Vec<Vec<u8>> {
        self.stop.send(()).unwrap();
        self.task.await.unwrap();
        Arc::try_unwrap(self.requests).unwrap().into_inner()
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .build()
        .unwrap()
}

fn file(entry: &str) -> GateFile<'_> {
    GateFile {
        entry,
        app: "owned-app",
        user: "owned-user",
        project: "owned-project",
        file: "owned-started.txt",
    }
}

fn report() -> JsonlReporter {
    JsonlReporter::begin(
        &format!("unit_gate_read_{}", uuid::Uuid::new_v4().simple()),
        "protocol",
        json!({}),
    )
}

fn detail(report: &JsonlReporter, value: &str) -> Vec<Value> {
    std::fs::read_to_string(&report.path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["name"] == "gate file-content read" && row["value"] == value)
        .map(|row| serde_json::from_str(row["detail"].as_str().unwrap()).unwrap())
        .collect()
}

fn assert_only_same_read(requests: &[Vec<u8>]) {
    for request in requests {
        assert!(request.starts_with(b"POST /api/git/file-content HTTP/1.1\r\n"));
        let offset = request
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap()
            + 4;
        let body: Value = serde_json::from_slice(&request[offset..]).unwrap();
        assert_eq!(
            body,
            json!({"workspaceType":"userApp","serviceType":"userapp",
            "appId":"owned-app","userId":"owned-user","cId":"owned-project",
            "projectId":"owned-project","ref":"worktree","filePath":"owned-started.txt"})
        );
    }
}

fn budget(request: Duration) -> PollBudget {
    PollBudget {
        total: Duration::from_secs(2),
        request,
        interval: Duration::from_millis(5),
    }
}

#[tokio::test]
async fn send_disconnect_or_reset_retries_only_read_and_preserves_first_cause() {
    for response in [Response::Disconnect, Response::Reset] {
        let server = Server::new(vec![response], Response::Json(200, STARTED)).await;
        let report = report();
        let (_sender, mut receiver) = watch::channel(true);
        poll_gate(
            &client(),
            &report,
            file(&server.entry),
            &mut receiver,
            budget(Duration::from_millis(500)),
        )
        .await
        .unwrap();
        let failed = detail(&report, "failed");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["stage"], "send");
        assert_eq!(failed[0]["reqwest_flags"]["is_request"], true);
        assert!(failed[0]["error_chain"].as_str().unwrap().contains(": "));
        let observed = detail(&report, "observed");
        assert_eq!(observed.last().unwrap()["transport_retry_count"], 1);
        let requests = server.finish().await;
        assert_eq!(requests.len(), 2);
        assert_only_same_read(&requests);
        assert!(report.finish());
    }
}

#[tokio::test]
async fn send_timeout_retries_within_original_budget_then_observes_gate() {
    let server = Server::new(vec![Response::Stall], Response::Json(200, STARTED)).await;
    let report = report();
    let (_sender, mut receiver) = watch::channel(true);
    poll_gate(
        &client(),
        &report,
        file(&server.entry),
        &mut receiver,
        budget(Duration::from_millis(80)),
    )
    .await
    .unwrap();
    let failed = detail(&report, "failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["reqwest_flags"]["is_timeout"], true);
    assert!(failed[0]["request_elapsed_ms"].as_u64().unwrap() >= 70);
    assert_eq!(
        detail(&report, "observed").last().unwrap()["transport_retry_count"],
        1
    );
    let requests = server.finish().await;
    assert_eq!(requests.len(), 2);
    assert_only_same_read(&requests);
    assert!(report.finish());
}

#[tokio::test]
async fn http_decode_validation_and_foreign_denial_do_not_retry() {
    for (response, stage) in [
        (Response::Json(503, "{}"), "http_status"),
        (Response::Json(403, "{}"), "http_status"),
        (Response::Json(200, "not-json"), "json_body"),
        (
            Response::Json(200, r#"{"success":false,"message":"foreign lifecycle"}"#),
            "validation",
        ),
        (Response::Json(200, r#"{"success":true}"#), "validation"),
    ] {
        let server = Server::new(vec![response], Response::Json(200, STARTED)).await;
        let report = report();
        let (_sender, mut receiver) = watch::channel(true);
        assert!(
            poll_gate(
                &client(),
                &report,
                file(&server.entry),
                &mut receiver,
                budget(Duration::from_millis(500))
            )
            .await
            .is_err()
        );
        let failed = detail(&report, "failed");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["stage"], stage);
        assert_eq!(failed[0]["retryable_read_transport"], false);
        let requests = server.finish().await;
        assert_eq!(requests.len(), 1);
        assert_only_same_read(&requests);
        assert!(report.finish());
    }
}

#[tokio::test]
async fn total_deadline_caps_last_request_and_keeps_last_transport_cause() {
    let server = Server::new(vec![], Response::Stall).await;
    let report = report();
    let (_sender, mut receiver) = watch::channel(true);
    let started = Instant::now();
    let result = poll_gate(
        &client(),
        &report,
        file(&server.entry),
        &mut receiver,
        PollBudget {
            total: Duration::from_millis(180),
            request: Duration::from_millis(70),
            interval: Duration::from_millis(15),
        },
    )
    .await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("readiness deadline"), "{error}");
    assert!(!error.ends_with("last transport cause: none"), "{error}");
    assert!(started.elapsed() < Duration::from_millis(700));
    assert!(
        detail(&report, "failed")
            .iter()
            .all(|row| row["request_timeout_ms"].as_u64().unwrap() <= 70)
    );
    let requests = server.finish().await;
    assert!((2..=3).contains(&requests.len()));
    assert_only_same_read(&requests);
    assert!(report.finish());
}

#[tokio::test]
async fn successful_file_alone_cannot_replace_stream_gate() {
    let server = Server::new(vec![], Response::Json(200, STARTED)).await;
    let report = report();
    let (_sender, mut receiver) = watch::channel(false);
    assert!(
        poll_gate(
            &client(),
            &report,
            file(&server.entry),
            &mut receiver,
            PollBudget {
                total: Duration::from_millis(80),
                request: Duration::from_millis(40),
                interval: Duration::from_millis(10)
            }
        )
        .await
        .is_err()
    );
    assert!(
        detail(&report, "observed")
            .iter()
            .all(|row| row["stream_started"] == false)
    );
    assert_only_same_read(&server.finish().await);
    assert!(report.finish());
}

#[tokio::test]
async fn invalid_request_builder_error_is_not_a_retryable_transport() {
    let report = report();
    let (_sender, mut receiver) = watch::channel(true);
    assert!(
        poll_gate(
            &client(),
            &report,
            file("://invalid"),
            &mut receiver,
            budget(Duration::from_millis(500))
        )
        .await
        .is_err()
    );
    let failed = detail(&report, "failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["reqwest_flags"]["is_builder"], true);
    assert_eq!(failed[0]["retryable_read_transport"], false);
    assert!(report.finish());
}
