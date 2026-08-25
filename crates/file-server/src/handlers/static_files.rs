//! 静态文件 HTTP handlers (对齐 nuwax `server.js` 顶层 `/api/page/static` + `/api/computer/static`)。
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

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tower::util::ServiceExt;
use tower_http::services::ServeFile;

use crate::AppState;
use crate::extract::{AppPath as AxumPath, AppQuery as Query};
use crate::workspace::ProjectContext;

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

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CustomTargetQuery {
    #[serde(default)]
    custom_target_dir: Option<String>,
}

// ── page static ────────────────────────────────────────────────────────────────

/// `GET|OPTIONS /api/page/static/{projectId}/{*rest}`
#[utoipa::path(
    get,
    path = "/static/{project_id}/{*rest}",
    params(
        ("project_id" = String, Path, description = "Project identifier"),
        ("rest" = String, Path, description = "Project-relative file path")
    ),
    responses(
        (status = 200, description = "Static file", body = crate::openapi::BinaryFile, content_type = "application/octet-stream"),
        (status = 404, description = "File not found")
    ),
    tag = "Static"
)]
pub(crate) async fn serve_page(
    State(state): State<AppState>,
    AxumPath((project_id, rest)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    if project_id.trim().is_empty() {
        return cors_404(&req, &PAGE_CORS);
    }
    let root = match state
        .resolver
        .resolve_project(&ProjectContext {
            project_id: project_id.to_string(),
            tenant_id: None,
            space_id: None,
            isolation_type: None,
        })
        .await
    {
        Ok(root) => root,
        Err(error) => return error.into_response(),
    };
    serve_from_root(&root, &rest, &PAGE_CORS, req).await
}

// ── userapp static ─────────────────────────────────────────────────────────────

/// `GET|OPTIONS /api/userapp/static/{appId}/{*rest}`（取整体包
/// `builds/workspace-package-{releaseId}.zip`——rest 即任务快照的 `artifactPath`）。
///
/// `app_id` 定位走 UserApp 开发卷（`resolve_userapp_dev`，与 build/detect/confirm 同根）。
/// 用 COMPUTER_CORS（暴露 Range/Content-Range，支持大产物断点续传）。
#[utoipa::path(
    get,
    path = "/static/{app_id}/{*rest}",
    params(
        ("app_id" = String, Path, description = "UserApp identifier (= workspace app_id)"),
        ("rest" = String, Path, description = "Workspace-relative file path (e.g. builds/workspace-package-{releaseId}.zip)")
    ),
    responses(
        (status = 200, description = "Static file", body = crate::openapi::BinaryFile, content_type = "application/octet-stream"),
        (status = 404, description = "File not found")
    ),
    tag = "UserApp"
)]
pub(crate) async fn serve_userapp(
    State(state): State<AppState>,
    AxumPath((app_id, rest)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    if app_id.trim().is_empty() {
        return cors_404(&req, &COMPUTER_CORS);
    }
    let root = match crate::workspace::resolve_userapp_dev(&app_id, None, &state.config) {
        Ok(root) => root,
        Err(error) => return error.into_response(),
    };
    serve_from_root(&root, &rest, &COMPUTER_CORS, req).await
}

// ── computer static ────────────────────────────────────────────────────────────

/// `GET|OPTIONS /api/computer/static/{userId}/{cId}/{*rest}?customTargetDir=`
#[utoipa::path(
    get,
    path = "/static/{user_id}/{c_id}/{*rest}",
    params(
        ("user_id" = String, Path, description = "User identifier"),
        ("c_id" = String, Path, description = "Computer workspace identifier"),
        ("rest" = String, Path, description = "Workspace-relative file path"),
        ("customTargetDir" = Option<String>, Query, description = "Override workspace root")
    ),
    responses(
        (status = 200, description = "Static file", body = crate::openapi::BinaryFile, content_type = "application/octet-stream"),
        (status = 404, description = "File not found")
    ),
    tag = "Static"
)]
pub(crate) async fn serve_computer(
    State(state): State<AppState>,
    AxumPath((user_id, c_id, rest)): AxumPath<(String, String, String)>,
    Query(q): Query<CustomTargetQuery>,
    req: Request,
) -> Response {
    if user_id.trim().is_empty() || c_id.trim().is_empty() {
        return cors_404(&req, &COMPUTER_CORS);
    }
    // userApp 分流与 computer 定位经公共收口（与 ws_path 单头，防两处漂移）
    let default_root =
        match super::computer::computer_root_for_request(&state, &user_id, &c_id).await {
            Ok(root) => root,
            Err(error) => return error.into_response(),
        };
    // customTargetDir 非空 → 完全覆盖根 (对齐 nuwax, 不拼 user/cId);
    // 注: 不做根目录白名单限制 —— 容器内内网部署, 且用户客户端复用本模块逻辑,
    // 每个用户电脑上的路径各不相同, 限制根路径会误伤正常业务。
    let root = match q
        .custom_target_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(ct) => PathBuf::from(ct),
        None => default_root,
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
        Err(_) => cors_404_static(origin.as_deref(), cors),
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

fn cors_404_static(origin: Option<&str>, cors: &CorsConfig) -> Response {
    add_cors_headers(
        (StatusCode::NOT_FOUND, "Not Found").into_response(),
        origin,
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
