//! Real UserApp handlers over TCP, consumed through the production forwarder.
//! The observer copies the original POST response; it does not supply a fixture
//! response, create a task, or buffer the SSE path.
use std::{sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::Response,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::forward_to_addr;

struct CapturedPost {
    status: StatusCode,
    body: Vec<u8>,
}
type Capture = Arc<Mutex<Option<CapturedPost>>>;

async fn observe_post(State(capture): State<Capture>, request: Request, next: Next) -> Response {
    let is_post = request.method() == Method::POST;
    let response = next.run(request).await;
    if !is_post {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 128 * 1024)
        .await
        .expect("bounded production response");
    *capture.lock().await = Some(CapturedPost {
        status: parts.status,
        body: bytes.to_vec(),
    });
    Response::from_parts(parts, Body::from(bytes))
}

struct Upstream(tokio::task::JoinHandle<()>);
impl Drop for Upstream {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn forwarded(
    addr: &str,
    method: Method,
    path: &str,
    last_event_id: Option<u64>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method(method.clone())
        .uri(path)
        .header(shared_types::APP_ID_HEADER, "forward-app")
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(id) = last_event_id {
        request = request.header("last-event-id", id.to_string());
    }
    let body = if method == Method::POST {
        Body::from(json!({"app_id":"forward-app"}).to_string())
    } else {
        Body::empty()
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        let response = forward_to_addr(
            "diagnostic fixture",
            "forward-app",
            addr,
            &shared_types::FileServerRequestCredentials::default(),
            request.body(body).unwrap(),
        )
        .await;
        let (parts, body) = response.into_parts();
        (
            parts.status,
            parts.headers,
            to_bytes(body, 128 * 1024).await.unwrap().to_vec(),
        )
    })
    .await
    .expect("forwarded response, including terminal SSE, must finish")
}

#[tokio::test]
async fn production_forwarder_preserves_real_precheck_diagnostics_task_and_sse() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("中文工作区");
    let workspace = root.join("forward-app");
    std::fs::create_dir_all(workspace.join("frontend")).unwrap();
    std::fs::write(
        workspace.join("workspace.manifest.toml"),
        "schema_version=1\n[workspace]\nname='forward-test'\n",
    )
    .unwrap();
    // An actual validation error embeds a Chinese value in its reason. No
    // handler is replaced, and the command must never reach execution.
    std::fs::write(workspace.join("frontend/project.manifest.toml"), "schema_version=1\n[project]\nservice_id='frontend'\nname='前端'\ntype='node'\n[build]\ncommand=['sh', '-c', 'touch MUST_NOT_RUN']\nartifact='/错误产物路径'\n[run]\ncommand=['true']\n").unwrap();
    let server = file_server::FileServer::builder(file_server::Config {
        userapp_workspace_dir: root,
        log_base_dir: directory.path().join("logs"),
        ..file_server::Config::default()
    })
    .build()
    .unwrap();
    let capture: Capture = Arc::new(Mutex::new(None));
    let router = file_server_userapp::full_router(&server).unwrap().layer(
        axum::middleware::from_fn_with_state(capture.clone(), observe_post),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let mut upstream = Upstream(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));

    for route in ["build", "dev/start", "dev/restart"] {
        let (status, _, bytes) = forwarded(
            &address,
            Method::POST,
            &format!("/api/v1/userapp/{route}"),
            None,
        )
        .await;
        let original = capture
            .lock()
            .await
            .take()
            .expect("real upstream handler was called");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(status, original.status);
        assert_eq!(
            bytes, original.body,
            "the complete production error envelope must survive forwarding"
        );
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope["success"], false);
        assert_eq!(
            envelope["code"],
            shared_types::error_codes::ERR_WORKSPACE_NO_SERVICES
        );
        assert!(
            envelope["message"]
                .as_str()
                .unwrap()
                .contains("错误产物路径")
        );
        let data = &envelope["data"];
        assert_eq!(data["status"], "failed");
        let id = data["task_id"]
            .as_str()
            .expect("production store generated a real task ID");
        let diagnostic = &data["diagnostics"][0];
        assert_eq!(diagnostic["code"], "manifest_validation");
        assert_eq!(diagnostic["phase"], "precheck");
        assert_eq!(diagnostic["repair_target"], "project");
        assert_eq!(diagnostic["scope"], "service");
        assert_eq!(diagnostic["service_id"], "frontend");
        assert_eq!(diagnostic["field"], "build.artifact");
        assert!(
            diagnostic["message"]
                .as_str()
                .unwrap()
                .contains("错误产物路径")
        );
        assert!(
            diagnostic["workspace_root"]
                .as_str()
                .unwrap()
                .contains("中文工作区")
        );

        let (status, _, bytes) = forwarded(
            &address,
            Method::GET,
            &format!("/api/v1/userapp/tasks/{id}?app_id=forward-app"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let snapshot: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            snapshot["success"], true,
            "successful query does not mean successful task"
        );
        assert_eq!(snapshot["data"]["id"], id);
        assert_eq!(snapshot["data"]["status"], "failed");
        assert_eq!(snapshot["data"]["diagnostics"], data["diagnostics"]);

        let path = format!("/api/v1/userapp/tasks/{id}/logs/stream?app_id=forward-app");
        let (status, headers, bytes) = forwarded(&address, Method::GET, &path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        let sse = String::from_utf8(bytes).unwrap();
        assert_eq!(sse.matches("event: failed").count(), 1);
        assert!(sse.find("event: log").unwrap() < sse.find("event: failed").unwrap());
        let events: Vec<Value> = sse
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        assert!(events.iter().any(|event| event["event"] == "log"
            && event["service"] == "frontend"
            && event["line"].as_str().is_some_and(
                |line| line.contains("源码预检查失败") && line.contains("错误产物路径")
            )));
        assert_eq!(events.last().unwrap()["event"], "failed");
        let ids: Vec<u64> = sse
            .lines()
            .filter_map(|line| line.strip_prefix("id: "))
            .map(|id| id.parse().unwrap())
            .collect();
        assert_eq!(ids, (0..events.len() as u64).collect::<Vec<_>>());
        let (_, _, after_terminal) =
            forwarded(&address, Method::GET, &path, ids.last().copied()).await;
        assert!(
            after_terminal.is_empty(),
            "forwarded Last-Event-ID must not replay terminal again"
        );
        assert!(!workspace.join("frontend/MUST_NOT_RUN").exists());
    }
    upstream.0.abort();
    assert!((&mut upstream.0).await.unwrap_err().is_cancelled());
}
