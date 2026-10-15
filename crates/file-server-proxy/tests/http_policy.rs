#![cfg(feature = "embed-file-server")]

use axum::{
    http::HeaderMap,
    response::IntoResponse,
    routing::{any, post},
};
use file_server_proxy::{FileServerProxyConfig, RoutePolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProbeObservation {
    probe: Vec<String>,
    cookie: Vec<String>,
    token: Vec<String>,
    hop: Vec<String>,
    authorization: Vec<String>,
    proxy_authorization: Vec<String>,
}

async fn probe(headers: HeaderMap) -> impl IntoResponse {
    let values = |name: &str| {
        headers
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let mut response = axum::Json(ProbeObservation {
        probe: values("x-probe"),
        cookie: values("cookie"),
        token: values("x-proxy-token"),
        hop: values("x-local-hop"),
        authorization: values("authorization"),
        proxy_authorization: values("proxy-authorization"),
    })
    .into_response();
    response
        .headers_mut()
        .insert("connection", "x-response-hop".parse().unwrap());
    response
        .headers_mut()
        .insert("x-response-hop", "private".parse().unwrap());
    response
        .headers_mut()
        .insert("x-proxy-token", "private".parse().unwrap());
    response
        .headers_mut()
        .insert("proxy-authenticate", "Basic realm=proxy".parse().unwrap());
    response.headers_mut().insert(
        "proxy-authentication-info",
        "nextnonce=private".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("www-authenticate", "Bearer realm=business".parse().unwrap());
    response
        .headers_mut()
        .append("set-cookie", "first=1".parse().unwrap());
    response
        .headers_mut()
        .append("set-cookie", "second=2".parse().unwrap());
    response
}

async fn request(address: &str) -> (String, ProbeObservation) {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(b"GET /api/git/status HTTP/1.1\r\nHost: localhost\r\nConnection: close, x-local-hop\r\nX-Proxy-Token: test-private-token\r\nX-Local-Hop: private\r\nX-Probe: first\r\nX-Probe: second\r\nCookie: a=1\r\nCookie: b=2\r\nAuthorization: Bearer business-canary\r\nProxy-Authorization: Basic proxy-canary\r\n\r\n").await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (headers, body) = text.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
    (headers.to_owned(), serde_json::from_str(body).unwrap())
}

#[tokio::test]
async fn embed_and_forward_preserve_duplicate_headers_without_hop_credentials() {
    let host_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_hits = host_hits.clone();
    let router = axum::Router::new()
        .route("/api/git/status", any(probe))
        .route(
            "/api/system/file-server/stop",
            post(move || {
                let hits = captured_hits.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    "host-management"
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let upstream_router = router.clone();
    let upstream =
        tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });
    file_server_proxy::init(FileServerProxyConfig {
        listen_host: "127.0.0.1".into(),
        listen_port: 0,
        rust_upstream_port: upstream_port,
        ts_upstream_port: upstream_port,
        policy: RoutePolicy::AllRust,
        auth_token: Some("test-private-token".into()),
        public_bind_declared: false,
        coordinated_dev_lifecycle: false,
    });
    let address = file_server_proxy::try_start().await.unwrap();
    let upstream_address = format!("127.0.0.1:{upstream_port}");
    assert_eq!(
        request_status(&upstream_address, "POST", "/api/system/file-server/stop").await,
        200
    );
    assert_eq!(
        request_status(&upstream_address, "POST", "/api/git/status").await,
        200
    );
    for embed in [true, false] {
        if embed {
            file_server_proxy::set_in_process_router(router.clone());
        } else {
            file_server_proxy::clear_in_process_router();
        }
        let (headers, body) = request(&address).await;
        assert_eq!(body.probe, ["first", "second"], "embed={embed}");
        assert_eq!(body.cookie, ["a=1", "b=2"], "embed={embed}");
        assert!(body.token.is_empty());
        assert!(body.hop.is_empty());
        assert!(body.proxy_authorization.is_empty());
        assert_eq!(body.authorization, ["Bearer business-canary"]);
        assert!(!headers.to_ascii_lowercase().contains("x-proxy-token:"));
        assert!(!headers.to_ascii_lowercase().contains("x-response-hop:"));
        assert!(!headers.to_ascii_lowercase().contains("proxy-authenticate:"));
        assert!(
            !headers
                .to_ascii_lowercase()
                .contains("proxy-authentication-info:")
        );
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("www-authenticate: bearer realm=business")
        );
        assert!(headers.to_ascii_lowercase().contains("set-cookie: first=1"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("set-cookie: second=2")
        );
        assert_eq!(
            request_status(&address, "POST", "/api/system/file-server/stop").await,
            404
        );
        assert_eq!(
            host_hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "denied management must not reach upstream, embed={embed}"
        );
        assert_eq!(
            request_status(&address, "POST", "/api/git/status").await,
            404,
            "extra upstream methods must not widen the public contract"
        );
    }
    file_server_proxy::stop().await.unwrap();
    upstream.abort();
}

async fn request_status(address: &str, method: &str, path: &str) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nX-Proxy-Token: test-private-token\r\nContent-Length: 0\r\n\r\n"
    );
    stream.write_all(wire.as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_string(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    response
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}
