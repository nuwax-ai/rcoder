//! 静态文件服务共享面：CORS 配置族 + serve_from_root + 响应辅助。
//!
//! serve_page / serve_computer 壳在 handlers/static_files.rs；
//! file-server-userapp 的 userapp static 取包复用本层——Range 断点续传走
//! COMPUTER_CORS 头集。

use std::path::Path;

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tower::util::ServiceExt;
use tower_http::services::ServeFile;

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
    let full = match crate::path_safety::ensure_within(root, &decoded) {
        Ok(p) => p,
        Err(_) => return cors_404(&req, cors),
    };
    if req.method() == axum::http::Method::OPTIONS {
        return cors_empty(&req, cors);
    }
    if !full.is_file() {
        return cors_404(&req, cors);
    }
    // ServeFile 处理 Range / ETag / Last-Modified / conditional GET
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let serve = ServeFile::new(full);
    match serve.oneshot(req).await {
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
}
