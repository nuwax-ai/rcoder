//! 静态文件服务路由 (对齐 nuwax `server.js` 顶层 `/api/page/static` + `/api/computer/static`)。
//!
//! 复刻要点 (nuwax 用 `res.sendFile`/`send` 库):
//! - 根目录: page = `PROJECT_SOURCE_DIR/{projectId}`; computer = `COMPUTER_WORKSPACE_DIR/{userId}/{cId}`
//!   (或 `?customTargetDir=` 完全覆盖根)
//! - 路径循环 `decodeURIComponent` 直到稳定 (safeDecodePath)
//! - dotfiles: allow
//! - CORS: 回显 Origin (无则 `*`), Origin 存在时附 Credentials+Vary; 两套路由 Allow/Expose 头不同
//! - OPTIONS 预检 → 200 空 body
//! - 文件不存在 → 404 `Not Found`
//! - Range/ETag/Last-Modified: 由 `tower_http::services::ServeFile` 处理

use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use tower::util::ServiceExt;
use tower_http::services::ServeFile;

use crate::AppState;
use crate::workspace::{ComputerContext, ProjectContext};

/// CORS 头配置 (两套路由不同)。
struct CorsConfig {
    allow_headers: &'static str,
    expose_headers: &'static str,
}

const PAGE_CORS: CorsConfig = CorsConfig {
    allow_headers: "Origin, X-Requested-With, Content-Type, Accept, Authorization, Cache-Control, Fragment",
    expose_headers: "Content-Type",
};

const COMPUTER_CORS: CorsConfig = CorsConfig {
    allow_headers: "Origin, X-Requested-With, Content-Type, Accept, Authorization, Cache-Control, Range, If-Range",
    expose_headers: "Content-Type, Content-Length, Content-Range, Accept-Ranges, ETag, Last-Modified",
};

const ALLOW_METHODS: &str = "HEAD,GET,POST,PUT,DELETE,OPTIONS";

/// 挂 `/api/page` 路由 (static 子路由)。
pub fn page_router() -> Router<AppState> {
    Router::new().route(
        "/static/{project_id}/{*rest}",
        get(serve_page).options(serve_page),
    )
}

/// computer static 路由 (挂到现有 `/api/computer` router 下, 相对 `/static/...`)。
pub fn computer_static_route() -> Router<AppState> {
    Router::new().route(
        "/static/{user_id}/{c_id}/{*rest}",
        get(serve_computer).options(serve_computer),
    )
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CustomTargetQuery {
    #[serde(default)]
    custom_target_dir: Option<String>,
}

// ── page static ────────────────────────────────────────────────────────────────

/// `GET|OPTIONS /api/page/static/{projectId}/{*rest}`
async fn serve_page(
    State(state): State<AppState>,
    AxumPath((project_id, rest)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    if project_id.trim().is_empty() {
        return cors_404(&req, &PAGE_CORS);
    }
    let root = state.resolver.resolve_project(&ProjectContext {
        project_id: project_id.to_string(),
        tenant_id: None,
        space_id: None,
        isolation_type: None,
    });
    serve_from_root(&root, &rest, &PAGE_CORS, req).await
}

// ── computer static ────────────────────────────────────────────────────────────

/// `GET|OPTIONS /api/computer/static/{userId}/{cId}/{*rest}?customTargetDir=`
async fn serve_computer(
    State(state): State<AppState>,
    AxumPath((user_id, c_id, rest)): AxumPath<(String, String, String)>,
    Query(q): Query<CustomTargetQuery>,
    req: Request,
) -> Response {
    if user_id.trim().is_empty() || c_id.trim().is_empty() {
        return cors_404(&req, &COMPUTER_CORS);
    }
    // customTargetDir 非空 → 完全覆盖根 (对齐 nuwax, 不拼 user/cId)
    let root = match q
        .custom_target_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(ct) => PathBuf::from(ct),
        None => state.resolver.resolve_computer(&ComputerContext {
            user_id: user_id.to_string(),
            cid: c_id.to_string(),
        }),
    };
    serve_from_root(&root, &rest, &COMPUTER_CORS, req).await
}

// ── 核心: 从 root + 剩余路径服务文件 ───────────────────────────────────────────

/// 从 root + 剩余路径服务文件 (循环 decode + dotfiles allow + Range + CORS)。
async fn serve_from_root(root: &Path, rest: &str, cors: &CorsConfig, req: Request) -> Response {
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
        Err(_) => cors_404_static(cors),
    }
}

// ── CORS 辅助 ──────────────────────────────────────────────────────────────────

fn origin_value(req: &Request) -> Option<String> {
    req.headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn add_cors_headers(mut resp: Response, origin: Option<&str>, cors: &CorsConfig) -> Response {
    let allow_origin = origin.unwrap_or("*");
    let headers = resp.headers_mut();
    let _ = headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_str(allow_origin).unwrap_or(HeaderValue::from_static("*")),
    );
    let _ = headers.insert(
        HeaderName::from_static("access-control-allow-methods"),
        HeaderValue::from_static(ALLOW_METHODS),
    );
    let _ = headers.insert(
        HeaderName::from_static("access-control-allow-headers"),
        HeaderValue::from_str(cors.allow_headers).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    let _ = headers.insert(
        HeaderName::from_static("access-control-expose-headers"),
        HeaderValue::from_str(cors.expose_headers).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    if origin.is_some() {
        let _ = headers.insert(
            HeaderName::from_static("access-control-allow-credentials"),
            HeaderValue::from_static("true"),
        );
        let _ = headers.append(axum::http::header::VARY, HeaderValue::from_static("Origin"));
    }
    resp
}

fn cors_empty(req: &Request, cors: &CorsConfig) -> Response {
    let origin = origin_value(req);
    add_cors_headers(StatusCode::OK.into_response(), origin.as_deref(), cors)
}

fn cors_404(req: &Request, cors: &CorsConfig) -> Response {
    let origin = origin_value(req);
    add_cors_headers(
        (StatusCode::NOT_FOUND, "Not Found").into_response(),
        origin.as_deref(),
        cors,
    )
}

fn cors_404_static(cors: &CorsConfig) -> Response {
    add_cors_headers(
        (StatusCode::NOT_FOUND, "Not Found").into_response(),
        None,
        cors,
    )
}

// ── 路径处理 ───────────────────────────────────────────────────────────────────

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
}
