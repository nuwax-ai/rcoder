//! 静态文件服务共享面：CORS 配置族 + serve_from_root + 响应辅助。
//!
//! serve_page / serve_computer 壳在 handlers/static_files.rs；
//! file-server-userapp 的 userapp static 取包复用本层——Range 断点续传走
//! COMPUTER_CORS 头集。

use std::path::Path;

use axum::extract::{OriginalUri, Request};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tower::util::ServiceExt;
use tower_http::services::ServeFile;

use crate::error::AppError;

/// CORS 头配置 (两套路由不同)。
pub struct CorsConfig {
    pub allow_headers: &'static str,
    pub expose_headers: &'static str,
}

pub const PAGE_CORS: CorsConfig = CorsConfig {
    allow_headers: "Origin, X-Requested-With, Content-Type, Accept, Authorization, Cache-Control, Fragment",
    expose_headers: "Content-Type",
};

pub const COMPUTER_CORS: CorsConfig = CorsConfig {
    allow_headers: "Origin, X-Requested-With, Content-Type, Accept, Authorization, Cache-Control, Range, If-Range",
    expose_headers: "Content-Type, Content-Length, Content-Range, Accept-Ranges, ETag, Last-Modified",
};

const ALLOW_METHODS: &str = "HEAD,GET,POST,PUT,DELETE,OPTIONS";

/// 从 root + 剩余路径服务文件 (循环 decode + dotfiles allow + Range + CORS)。
pub async fn serve_from_root(root: &Path, rest: &str, cors: &CorsConfig, req: Request) -> Response {
    let relative = rest.trim_start_matches('/');
    let decoded = safe_decode_path(relative);
    if decoded.is_empty() {
        return cors_404(&req, cors);
    }
    // 路径安全: 仅防穿越 (dotfiles allow, 不拦隐藏名)
    let full = match crate::path_safety::ensure_resolved_within(root, &decoded).await {
        Ok(p) => p,
        Err(_) => return cors_404(&req, cors),
    };
    if req.method() == axum::http::Method::OPTIONS {
        return cors_empty(&req, cors);
    }
    let origin = origin_value(&req);
    // Axum strips nested router prefixes from `Request::uri()`. Error responses
    // should retain the public URL (as Express `sendFile` errors do), so prefer
    // its original URI and keep the nested URI only for direct service tests.
    let request_path = req
        .extensions()
        .get::<OriginalUri>()
        .map(|uri| uri.0.path())
        .unwrap_or_else(|| req.uri().path())
        .to_owned();
    // tokio::fs::metadata 而非阻塞的 Path::is_file()：本函数每次静态请求都会走到，
    // 阻塞调用会占住 tokio worker。NotFound 使用与 TS 一致的结构化资源错误；其他
    // I/O 错误按种类传播，不能把权限/设备故障伪装成文件缺失。
    match tokio::fs::metadata(&full).await {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return cors_404(&req, cors),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return cors_resource_404(&request_path, origin.as_deref(), cors);
        }
        Err(error) => {
            let response = AppError::from(error).into_response();
            return add_cors_headers(response, origin.as_deref(), cors);
        }
    }
    // ServeFile 处理 Range / ETag / Last-Modified / conditional GET
    let serve = ServeFile::new(full);
    match serve.oneshot(req).await {
        Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {
            cors_resource_404(&request_path, origin.as_deref(), cors)
        }
        Ok(resp) => add_cors_headers(resp.into_response(), origin.as_deref(), cors),
        Err(_) => cors_404_static(origin.as_deref(), cors),
    }
}

pub fn origin_value(req: &Request) -> Option<String> {
    req.headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

pub fn add_cors_headers(mut resp: Response, origin: Option<&str>, cors: &CorsConfig) -> Response {
    let allow_origin = origin.unwrap_or("*");
    let headers = resp.headers_mut();
    drop(headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str(allow_origin).unwrap_or(HeaderValue::from_static("*")),
    ));
    drop(headers.insert(
        HeaderName::from_static("access-control-allow-methods"),
        HeaderValue::from_static(ALLOW_METHODS),
    ));
    drop(headers.insert(
        HeaderName::from_static("access-control-allow-headers"),
        HeaderValue::from_str(cors.allow_headers).unwrap_or_else(|_| HeaderValue::from_static("")),
    ));
    drop(headers.insert(
        HeaderName::from_static("access-control-expose-headers"),
        HeaderValue::from_str(cors.expose_headers).unwrap_or_else(|_| HeaderValue::from_static("")),
    ));
    // 注: CORS 凭据策略不在本层拦截 —— 前置有网关/代理系统,
    // 需要收紧时由前置系统统一处理, file-server 保持原行为 (有 Origin 即下发凭据头)。
    if origin.is_some() {
        drop(headers.insert(
            HeaderName::from_static("access-control-allow-credentials"),
            HeaderValue::from_static("true"),
        ));
        headers.append(axum::http::header::VARY, HeaderValue::from_static("Origin"));
    }
    resp
}

fn cors_empty(req: &Request, cors: &CorsConfig) -> Response {
    let origin = origin_value(req);
    add_cors_headers(StatusCode::OK.into_response(), origin.as_deref(), cors)
}

fn cors_resource_404(path: &str, origin: Option<&str>, cors: &CorsConfig) -> Response {
    let response = AppError::resource(format!("Path not found: {path}")).into_response();
    add_cors_headers(response, origin, cors)
}

pub fn cors_404(req: &Request, cors: &CorsConfig) -> Response {
    let origin = origin_value(req);
    add_cors_headers(
        (StatusCode::NOT_FOUND, "Not Found").into_response(),
        origin.as_deref(),
        cors,
    )
}

fn cors_404_static(origin: Option<&str>, cors: &CorsConfig) -> Response {
    add_cors_headers(
        (StatusCode::NOT_FOUND, "Not Found").into_response(),
        origin,
        cors,
    )
}

/// 循环 percent-decode 直到稳定 (对齐 nuwax safeDecodePath); 上限 8 轮防恶意循环。
fn safe_decode_path(s: &str) -> String {
    let mut prev = s.to_string();
    for _ in 0..8 {
        let next = crate::service::code::decode_uri_component(&prev);
        if next == prev {
            break;
        }
        prev = next;
    }
    prev
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_decode_path_loops_until_stable() {
        assert_eq!(safe_decode_path("a%20b"), "a b");
        assert_eq!(safe_decode_path("%E4%B8%AD"), "中");
        assert_eq!(safe_decode_path("foo/bar.js"), "foo/bar.js");
    }

    #[test]
    fn ensure_within_rejects_traversal_allows_dotfiles() {
        let root = Path::new("/app/ws");
        assert!(crate::path_safety::ensure_within(root, "../etc").is_err());
        assert!(crate::path_safety::ensure_within(root, "/etc/passwd").is_err());
        assert!(crate::path_safety::ensure_within(root, "src/a.js").is_ok());
        // dotfiles allow: .env / .git/config 不被隐藏拦截 (ensure_within 只防穿越)
        assert!(crate::path_safety::ensure_within(root, ".env").is_ok());
        assert!(crate::path_safety::ensure_within(root, ".git/config").is_ok());
    }

    #[test]
    fn cors_headers_echo_origin_with_credentials() {
        // 有 Origin: 回显 + 凭据头 (CORS 拦截由前置网关负责, 本层不做)
        let resp = add_cors_headers(
            StatusCode::OK.into_response(),
            Some("http://client.example.com"),
            &PAGE_CORS,
        );
        assert_eq!(
            resp.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "http://client.example.com"
        );
        assert_eq!(
            resp.headers().get("access-control-allow-credentials"),
            Some(&HeaderValue::from_static("true"))
        );
        // 无 Origin: `*` 且无凭据头
        let resp = add_cors_headers(StatusCode::OK.into_response(), None, &PAGE_CORS);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
        assert!(
            resp.headers()
                .get("access-control-allow-credentials")
                .is_none()
        );
    }
    #[tokio::test]
    async fn native_path_link_reads_reject_outside_and_preserve_inside() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("workspace");
        let inside = base.join("inside");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(inside.join("sentinel.txt"), "inside").unwrap();
        std::fs::write(outside.join("sentinel.txt"), "outside").unwrap();
        let mut outcomes = Vec::new();
        for (name, target) in [("internal", &inside), ("external", &outside)] {
            let link = base.join(name);
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &link).unwrap();
            #[cfg(windows)]
            assert!(
                std::process::Command::new("cmd.exe")
                    .args(["/d", "/c", "mklink", "/J"])
                    .arg(&link)
                    .arg(target)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            let relative = format!("{name}/sentinel.txt");
            let resolved =
                crate::service::tree::resolve::resolve_existing_file(&base, &relative, None)
                    .await
                    .unwrap()
                    .is_some();
            let response = serve_from_root(
                &base,
                &relative,
                &COMPUTER_CORS,
                Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            #[cfg(unix)]
            std::fs::remove_file(&link).unwrap();
            #[cfg(windows)]
            std::fs::remove_dir(&link).unwrap();
            outcomes.push((resolved, status, body));
        }
        assert!(outcomes[0].0, "internal link remains readable");
        assert_eq!(outcomes[0].1, StatusCode::OK);
        assert_eq!(outcomes[0].2.as_ref(), b"inside");
        assert!(!outcomes[1].0, "external link resolves to exists:false");
        assert_eq!(outcomes[1].1, StatusCode::NOT_FOUND);
        assert_ne!(outcomes[1].2.as_ref(), b"outside");
    }

    #[tokio::test]
    async fn missing_static_file_returns_structured_resource_error_with_cors() {
        let root = tempfile::tempdir().unwrap();
        let path = "/api/page/static/project/missing.txt";
        let request = Request::builder()
            .uri("/static/project/missing.txt")
            .header("origin", "https://client.example")
            .body(axum::body::Body::empty())
            .unwrap();
        let (mut parts, body) = request.into_parts();
        parts.extensions.insert(OriginalUri(path.parse().unwrap()));
        let request = Request::from_parts(parts, body);

        let response = serve_from_root(root.path(), "missing.txt", &PAGE_CORS, request).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://client.example"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["code"], "UNKNOWN_ERROR");
        assert_eq!(value["error"]["type"], "RESOURCE_ERROR");
        assert_eq!(value["error"]["message"], format!("Path not found: {path}"));
    }
}
