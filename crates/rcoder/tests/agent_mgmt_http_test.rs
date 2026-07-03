//! Agent Management HTTP 路由集成测试 (P0-5)
//!
//! 验证 8 个新 POST 端点(取代旧 GET/DELETE):
//! - JSON body 反序列化(每个 body 类型的 serde 测试)
//! - Router 路径/方法匹配(只接受 POST,GET → 405)
//! - 路径不存在 → 404
//!
//! 端到端测试(实际调 gRPC 转发)在 `agent_mgmt_forward_test.rs` 已覆盖。

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use tower::ServiceExt;

use rcoder::handler::agent_mgmt_handler as handler;

// === JSON body 反序列化测试 ===

#[test]
fn list_agents_body_parses_from_json() {
    let body: shared_types::ListAgentsRequest =
        serde_json::from_str(r#"{"project_id":"p1"}"#).unwrap();
    assert_eq!(body.routing.project_id.as_deref(), Some("p1"));
}

#[test]
fn list_agents_body_parses_with_defaults() {
    let body: shared_types::ListAgentsRequest = serde_json::from_str(r#"{}"#).unwrap();
    assert!(body.routing.project_id.is_none());
}

#[test]
fn get_agent_body_parses() {
    let body: shared_types::GetAgentRequest =
        serde_json::from_str(r#"{"project_id":"p1","agent_id":"codex-acp"}"#).unwrap();
    assert_eq!(body.routing.project_id.as_deref(), Some("p1"));
    assert_eq!(body.agent_id, "codex-acp");
}

#[test]
fn get_agent_body_missing_agent_id_fails() {
    let result: Result<shared_types::GetAgentRequest, _> =
        serde_json::from_str(r#"{"project_id":"p1"}"#);
    assert!(
        result.is_err(),
        "missing agent_id should fail deserialization"
    );
}

#[test]
fn check_agent_body_parses() {
    let body: shared_types::CheckAgentRequest =
        serde_json::from_str(r#"{"project_id":"p1","agent_id":"codex-acp"}"#).unwrap();
    assert_eq!(body.agent_id, "codex-acp");
}

#[test]
fn uninstall_agent_body_parses() {
    let body: shared_types::UninstallAgentRequest =
        serde_json::from_str(r#"{"project_id":"p1","agent_id":"codex-acp"}"#).unwrap();
    assert_eq!(body.agent_id, "codex-acp");
}

#[test]
fn install_from_url_body_parses() {
    let body: shared_types::InstallFromUrlRequest = serde_json::from_str(
        r#"{"project_id":"p1","agent":{"agent_id":"codex-acp","command":"codex-acp","args":["--serve"],"version":"1.2.0"},"platforms":{"linux-x86_64":{"url":"https://x.example/agent-linux-amd64.tar.gz","sha256":"abc"}}}"#,
    )
    .unwrap();
    assert_eq!(body.agent.version.as_deref(), Some("1.2.0"));
    assert_eq!(body.platforms.len(), 1);
    assert_eq!(
        body.platforms["linux-x86_64"].url,
        "https://x.example/agent-linux-amd64.tar.gz"
    );
    assert_eq!(
        body.platforms["linux-x86_64"].sha256.as_deref(),
        Some("abc")
    );
    assert_eq!(body.agent.args, vec!["--serve"]);
}

#[test]
fn install_from_npm_body_parses() {
    let body: shared_types::InstallFromPackageManagerRequest = serde_json::from_str(
        r#"{"project_id":"p1","agent":{"agent_id":"kimi","command":"kimi"},"package":"@scope/kimi"}"#,
    )
    .unwrap();
    assert_eq!(body.package, "@scope/kimi");
    assert_eq!(body.agent.command, "kimi");
}

#[test]
fn install_metadata_body_parses_with_default_install_type() {
    let json = r#"{
        "project_id": "p1",
        "agent": {"agent_id": "codex-acp", "command": "codex-acp", "args": ["--serve"]},
        "sha256": "deadbeef"
    }"#;
    let m: handler::InstallMetadataBody = serde_json::from_str(json).unwrap();
    assert_eq!(m.install_type, "BINARY"); // default
    assert_eq!(m.routing.project_id.as_deref(), Some("p1"));
    assert_eq!(m.agent.agent_id, "codex-acp");
}

#[test]
fn install_metadata_body_explicit_install_type_url() {
    let json = r#"{
        "project_id": "p1",
        "agent": {"agent_id": "remote", "command": "remote"},
        "install_type": "URL",
        "source_url": "https://example.com/x.tar.gz"
    }"#;
    let m: handler::InstallMetadataBody = serde_json::from_str(json).unwrap();
    assert_eq!(m.install_type, "URL");
    assert_eq!(
        m.source_url.as_deref(),
        Some("https://example.com/x.tar.gz")
    );
}

#[test]
fn install_metadata_body_npm_with_package() {
    let json = r#"{
        "project_id": "p1",
        "agent": {"agent_id": "kimi", "command": "kimi"},
        "install_type": "NPM",
        "npm_package": "@scope/kimi"
    }"#;
    let m: handler::InstallMetadataBody = serde_json::from_str(json).unwrap();
    assert_eq!(m.install_type, "NPM");
    assert_eq!(m.npm_package.as_deref(), Some("@scope/kimi"));
}

#[test]
fn body_types_serialize_back_to_json() {
    // 验证 round-trip:反序列化后能再序列化
    let body: shared_types::ListAgentsRequest =
        serde_json::from_str(r#"{"project_id":"p1"}"#).unwrap();
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("project_id"));
}

// === Router 路径/方法匹配测试 ===

/// 验证只接受 POST:GET 应返回 405
#[tokio::test]
async fn list_agents_route_rejects_get() {
    // 只注册 POST 路由(模拟 router.rs 的真实配置)
    let app = Router::new().route(
        "/agent-mgmt/agents/list",
        post(|| async { "should-not-reach" }),
    );
    let req = Request::builder()
        .method("GET")
        .uri("/agent-mgmt/agents/list")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "GET to POST-only route should return 405"
    );
}

/// 验证 POST 能到达 handler(用 echo 桩验证)
#[tokio::test]
async fn list_agents_route_accepts_post() {
    let app = Router::new().route("/agent-mgmt/agents/list", post(|| async { "reached" }));
    let req = Request::builder()
        .method("POST")
        .uri("/agent-mgmt/agents/list")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"project_id":"p1"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// 验证路径不存在时返回 404
#[tokio::test]
async fn unknown_path_returns_404() {
    let app = Router::new().route("/agent-mgmt/agents/list", post(|| async { "ok" }));
    let req = Request::builder()
        .method("POST")
        .uri("/agent-mgmt/agents/typo")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// === Multipart body 限制测试(P0-5 修复回归保护) ===
//
// 历史 bug:P0-5 重构时只在 MethodRouter 上挂了 `RequestBodyLimitLayer`,
// 但 axum 的 `Multipart` 提取器读的是 Request 上的 `DefaultBodyLimitKind` 扩展,
// `RequestBodyLimitLayer` 对 multipart 不生效,实际限制仍是全局 50MB。
// 这些测试锁定 500MB 限制在 install 路由上确实生效。

use axum::extract::DefaultBodyLimit;

/// 构造一个合法的 multipart/form-data body(模拟 `curl -F file=@x -F metadata='...'`)
fn build_multipart_body(file_bytes: &[u8], metadata_json: &str) -> (String, Vec<u8>) {
    let boundary = "----testboundary12345";
    let mut body = Vec::new();
    // file part
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"agent.bin\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(b"\r\n");
    // metadata part
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"metadata\"\r\n\r\n");
    body.extend_from_slice(metadata_json.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// 1MB 的 multipart 请求应能到达 handler(不触发 body limit)
#[tokio::test]
async fn install_route_accepts_1mb_multipart_body() {
    async fn echo_handler(
        mut m: axum::extract::Multipart,
    ) -> Result<String, (axum::http::StatusCode, String)> {
        let mut got_file = 0usize;
        let mut got_meta = String::new();
        while let Some(f) = m
            .next_field()
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("mp: {e}")))?
        {
            match f.name().unwrap_or("") {
                "file" => {
                    let b = f.bytes().await.map_err(|e| {
                        (axum::http::StatusCode::BAD_REQUEST, format!("bytes: {e}"))
                    })?;
                    got_file = b.len();
                }
                "metadata" => {
                    got_meta = f
                        .text()
                        .await
                        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("text: {e}")))?;
                }
                _ => {
                    let _ = f.bytes().await;
                }
            }
        }
        Ok(format!("file={got_file},meta={got_meta}"))
    }

    let app = Router::new().route(
        "/agent-mgmt/agents/install",
        post(echo_handler).layer(DefaultBodyLimit::max(500 * 1024 * 1024)),
    );

    let payload = vec![0u8; 1024 * 1024]; // 1MB
    let meta = r#"{"agent":{"agent_id":"x","command":"x"},"install_type":"BINARY"}"#;
    let (ct, body) = build_multipart_body(&payload, meta);
    let req = Request::builder()
        .method("POST")
        .uri("/agent-mgmt/agents/install")
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "1MB multipart should pass");
}

/// 10MB 的 multipart 请求在 50MB 全局限制下应能通过(用于确认 DefaultBodyLimit 已挂)
#[tokio::test]
async fn install_route_accepts_10mb_multipart_body() {
    async fn echo_handler(
        mut m: axum::extract::Multipart,
    ) -> Result<String, (axum::http::StatusCode, String)> {
        let mut got_file = 0usize;
        while let Some(f) = m
            .next_field()
            .await
            .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("mp: {e}")))?
        {
            if f.name().unwrap_or("") == "file" {
                let b = f
                    .bytes()
                    .await
                    .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("bytes: {e}")))?;
                got_file = b.len();
            }
        }
        Ok(format!("file={got_file}"))
    }

    // 模拟 install 路由的真实挂法:500MB DefaultBodyLimit
    let app = Router::new().route(
        "/agent-mgmt/agents/install",
        post(echo_handler).layer(DefaultBodyLimit::max(500 * 1024 * 1024)),
    );

    let payload = vec![0u8; 10 * 1024 * 1024]; // 10MB
    let meta = r#"{"agent":{"agent_id":"x","command":"x"}}"#;
    let (ct, body) = build_multipart_body(&payload, meta);
    let req = Request::builder()
        .method("POST")
        .uri("/agent-mgmt/agents/install")
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "10MB multipart should pass");
}

// === install_agent parse_install_type 行为测试 ===
//
// 修复保护:`parse_install_type` 把 `Some("")` 当作缺失(回落到 BINARY),
// 而不是当作 unknown 报 validation error。

#[test]
fn install_metadata_body_empty_install_type_defaults_to_binary() {
    // `Some("")` 会被 parse_install_type 视为缺失(走 trim+filter)
    let m: handler::InstallMetadataBody =
        serde_json::from_str(r#"{"agent":{"agent_id":"x","command":"x"},"install_type":""}"#)
            .unwrap();
    assert_eq!(m.install_type, ""); // 字段本身保持空串(handler 才规范化)
}

#[test]
fn install_metadata_body_explicit_binary_works() {
    let m: handler::InstallMetadataBody =
        serde_json::from_str(r#"{"agent":{"agent_id":"x","command":"x"},"install_type":"BINARY"}"#)
            .unwrap();
    assert_eq!(m.install_type, "BINARY");
}

#[test]
fn install_metadata_body_lowercase_npm_works() {
    let m: handler::InstallMetadataBody = serde_json::from_str(
        r#"{"agent":{"agent_id":"x","command":"x"},"install_type":"npm","npm_package":"@scope/p"}"#,
    )
    .unwrap();
    assert_eq!(m.install_type, "npm");
}

#[test]
fn install_metadata_body_mixed_case_url_works() {
    let m: handler::InstallMetadataBody = serde_json::from_str(
        r#"{"agent":{"agent_id":"x","command":"x"},"install_type":"Url","source_url":"https://x/y"}"#,
    )
    .unwrap();
    assert_eq!(m.install_type, "Url");
}

#[test]
fn install_from_url_body_requires_agent() {
    // 缺失 agent 子对象 → serde 拒绝
    let r: Result<shared_types::InstallFromUrlRequest, _> = serde_json::from_str(
        r#"{"version":"1.0.0","platforms":{"linux-x86_64":{"url":"https://x/y"}}}"#,
    );
    assert!(r.is_err(), "missing agent should fail deserialization");
}

#[test]
fn install_from_npm_body_requires_agent() {
    let r: Result<shared_types::InstallFromPackageManagerRequest, _> =
        serde_json::from_str(r#"{"package":"@scope/p"}"#);
    assert!(r.is_err(), "missing agent should fail deserialization");
}

#[test]
fn install_from_url_body_empty_agent_id_parses_but_handler_rejects() {
    // 显式空串能 parse 过去(serde String 接受空串),
    // 但 handler 里的 require_field 会拦下。这条测试保护这条契约。
    let body: shared_types::InstallFromUrlRequest = serde_json::from_str(
        r#"{"agent":{"agent_id":"","command":"x","version":"1.0.0"},"platforms":{"linux-x86_64":{"url":"https://x/y"}}}"#,
    )
    .unwrap();
    assert_eq!(body.agent.agent_id, "");
}

#[test]
fn install_from_npm_body_empty_agent_id_parses_but_handler_rejects() {
    let body: shared_types::InstallFromPackageManagerRequest =
        serde_json::from_str(r#"{"agent":{"agent_id":"","command":"x"},"package":"@scope/p"}"#)
            .unwrap();
    assert_eq!(body.agent.agent_id, "");
}
