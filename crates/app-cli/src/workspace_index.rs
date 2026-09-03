//! workspace 根静态首页服务（`index.html`）。
//!
//! pingap 是纯反代（`LocationConf` 无静态文件能力），workspace 根的
//! `index.html` 由本服务承载：监听 `0.0.0.0:9081`（在 lock 端口分配范围
//! 4000-7999 之外，不占服务端口池），pingap 在无服务占用 `/`（catch-all）
//! 时注入一条无 path 兜底路由指向本服务——访问入口根路径/未匹配路径即展示
//! 该页面。仅服务 workspace **根一级**文件（一级子目录是各服务源码，不暴露），
//! 每次请求实时读文件：用户改 `index.html` 刷新即生效。

use std::path::{Component, Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

/// 静态首页端口（9080 pingap 入口相邻；不参与 lock 端口分配）。
pub const INDEX_PORT: u16 = 9081;

/// workspace 首页文件名（根一级）。
const INDEX_FILE: &str = "index.html";

/// workspace 首页兜底路由是否生效（与 supervisor 起服务/编译器注入的判定单一事实源）：
/// `index.html` 存在 且 无 enabled 服务声明 `[proxy].path == "/"`。
///
/// 有 catch-all 服务时返回 None（服务声明优先；pingap 无 path location 权重 0，
/// 两个并存同权重有匹配歧义），warn 说明。
pub fn index_port_if_eligible(
    workspace: &Path,
    services: &[crate::manifest::ServiceSpec],
) -> Option<u16> {
    if !workspace.join(INDEX_FILE).is_file() {
        return None;
    }
    let catchall = services.iter().find(|service| {
        service.enabled
            && service
                .proxy
                .as_ref()
                .is_some_and(|proxy| proxy.path == "/")
    });
    if let Some(service) = catchall {
        tracing::warn!(
            "index.html exists but service '{}' declares [proxy].path = \"/\" \
             (catch-all): the service route takes priority, index.html is NOT served",
            service.service_id
        );
        return None;
    }
    Some(INDEX_PORT)
}

/// 静态首页服务是否已在本进程内启动（幂等标记：builtin 热部署重编排 /
/// supervisord 引擎重复 orchestrate 不得二次 bind 同端口）。
static SPAWNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动静态首页服务（幂等）：已在跑则直接 Ok；bind 失败回滚标记并上抛
/// （端口被外部占用 = 编排应 fail-fast，静默跳过会让兜底路由 502）。
///
/// 后台 task 常驻 app-cli 进程生命周期（热部署换 code 后实时读文件自然
/// 切到新内容，无需重启）；workspace 路径热部署间恒定（变的是目录内容）。
pub fn ensure_spawned(workspace: &Path) -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    if SPAWNED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let spawn_result = (|| -> anyhow::Result<()> {
        let listener = std::net::TcpListener::bind(("0.0.0.0", INDEX_PORT)).map_err(|e| {
            anyhow::anyhow!("bind workspace index server 0.0.0.0:{INDEX_PORT}: {e}")
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| anyhow::anyhow!("workspace index listener nonblocking: {e}"))?;
        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|e| anyhow::anyhow!("workspace index listener async: {e}"))?;
        let router = router(workspace.to_path_buf());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!("workspace index server exited: {e}");
            }
        });
        Ok(())
    })();
    if spawn_result.is_err() {
        SPAWNED.store(false, Ordering::SeqCst);
    }
    spawn_result
}

fn router(workspace: PathBuf) -> Router {
    Router::new()
        // get 仅匹配 GET，HEAD 需显式挂同 handler（curl -I / 探活工具用 HEAD）
        .route("/{*file}", get(serve).head(serve))
        .route("/", get(serve).head(serve))
        .with_state(workspace)
}

/// 根路径 → index.html；其余仅根一级文件（子目录 404，路径穿越 404）。
/// HEAD 与 GET 同判定，但按协议剥 body 只留头。
async fn serve(State(workspace): State<PathBuf>, method: axum::http::Method, uri: Uri) -> Response {
    let path = uri.path();
    if path.contains("..") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // URL 解码后的 path segments；"/" → index.html；仅接受根一级（单段）
    let rel = if path == "/" {
        PathBuf::from(INDEX_FILE)
    } else {
        let decoded = percent_decode(path);
        let mut segments = Path::new(&decoded).components();
        let mut rel = PathBuf::new();
        for component in segments.by_ref() {
            match component {
                Component::Normal(seg) => {
                    rel.push(seg);
                    // 根一级即止：出现第二段 = 子目录 → 404
                    break;
                }
                // 前导 /（RootDir）与 ./（CurDir）跳过
                Component::RootDir | Component::CurDir => continue,
                Component::Prefix(_) | Component::ParentDir => {
                    return (StatusCode::NOT_FOUND, "not found").into_response();
                }
            }
        }
        // 剩余还有段 = 子目录路径
        if segments.next().is_some() {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
        rel
    };
    // 防御：文件名必须非空且不含路径分隔（子目录/穿越兜底）
    let Some(name) = rel.to_str().filter(|s| !s.is_empty() && !s.contains('/')) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let target = workspace.join(name);
    match tokio::fs::read(&target).await {
        Ok(bytes) => {
            let mut headers = HeaderMap::new();
            if let Some(mime) = content_type(name) {
                headers.insert(
                    header::CONTENT_TYPE,
                    mime.parse().unwrap_or_else(|_| {
                        "application/octet-stream".parse().expect("static mime")
                    }),
                );
            }
            if method == axum::http::Method::HEAD {
                // HEAD：只回头部（Content-Length 保持文件大小语义），body 为空
                headers.insert(
                    header::CONTENT_LENGTH,
                    bytes.len().to_string().parse().expect("decimal length"),
                );
                return (StatusCode::OK, headers, axum::body::Body::empty()).into_response();
            }
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// 百分号解码（path 里的 %20 等）；解码失败原样返回（后续文件读取 404 兜底）。
fn percent_decode(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // %XX 形式且两个 hex 字符都在界内（i+2 < len 保证 path[i+1..i+3] 合法）；
        // 非 %XX 或非法 hex 原样保留（后续文件读取 404 兜底）
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&path[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 常见静态扩展名的 Content-Type（未知扩展名省略头，HTTP 默认 octet-stream）。
fn content_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit('.').next()?;
    Some(match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "txt" | "md" => "text/plain; charset=utf-8",
        "woff2" => "font/woff2",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn temp_ws_with_index() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.keep();
        std::fs::write(
            path.join(INDEX_FILE),
            "<html><body>workspace home</body></html>",
        )
        .expect("write index");
        std::fs::write(path.join("logo.svg"), "<svg/>").expect("write svg");
        std::fs::create_dir_all(path.join("backend-go")).expect("subdir");
        std::fs::write(path.join("backend-go").join("server"), "binary").expect("sub file");
        path
    }

    async fn get_body(router: &Router, path: &str) -> (StatusCode, String, String) {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes()
            .to_vec();
        (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    fn spec(catchall: bool) -> crate::manifest::ServiceSpec {
        use workspace_manifest::{ProxySection, RunSection};
        crate::manifest::ServiceSpec {
            service_id: "frontend".into(),
            name: "Frontend".into(),
            dir: "frontend".into(),
            r#type: workspace_manifest::ProjectType::Node,
            kind: workspace_manifest::ProjectKind::Web,
            enabled: true,
            port: 4578,
            devbuild: None,
            run: RunSection {
                command: vec!["true".into()],
                migrate: Vec::new(),
                depends_on: Vec::new(),
                shutdown_timeout_seconds: 30,
            },
            devrun: None,
            static_content_dir: None,
            health: Default::default(),
            proxy: Some(ProxySection {
                path: if catchall { "/".into() } else { "/app".into() },
                strip_prefix: false,
                plugins: Vec::new(),
                upstream_includes: Vec::new(),
            }),
            logs: Vec::new(),
            env: Default::default(),
        }
    }

    /// 根路径返回 index.html（text/html）。
    #[tokio::test]
    async fn root_serves_index_html() {
        let ws = temp_ws_with_index();
        let router = router(ws);
        let (status, content_type, body) = get_body(&router, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/html"));
        assert!(body.contains("workspace home"));
    }

    /// 根一级静态文件可访问（含 Content-Type）。
    #[tokio::test]
    async fn root_level_file_served() {
        let ws = temp_ws_with_index();
        let router = router(ws);
        let (status, content_type, body) = get_body(&router, "/logo.svg").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "image/svg+xml");
        assert_eq!(body, "<svg/>");
    }

    /// HEAD 只回头部（Content-Length = 文件大小），body 为空。
    #[tokio::test]
    async fn head_returns_headers_without_body() {
        let ws = temp_ws_with_index();
        let router = router(ws);
        let response = router
            .oneshot(
                axum::http::Request::head("/")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let length = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .expect("content-length");
        assert_eq!(
            length,
            "<html><body>workspace home</body></html>".len().to_string()
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes();
        assert!(body.is_empty(), "HEAD must not carry a body");
    }

    /// 子目录与穿越路径 404（源码在一级子目录，不暴露）。
    #[tokio::test]
    async fn subdirectory_and_traversal_are_404() {
        let ws = temp_ws_with_index();
        let router = router(ws);
        for path in [
            "/backend-go/server",
            "/backend-go",
            "/..%2fbackend-go%2fserver",
            "/%2e%2e/workspace.manifest.toml",
            "/missing.txt",
        ] {
            let (status, _, _) = get_body(&router, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "path={path}");
        }
    }

    /// index_port_if_eligible：无 index → None；有 index 无 catch-all → Some(9081)；
    /// 有 index 但服务占 `/` → None（服务优先）。
    #[test]
    fn index_port_requires_index_and_no_catchall() {
        let ws = temp_ws_with_index();
        assert_eq!(
            index_port_if_eligible(&ws, &[spec(false)]),
            Some(INDEX_PORT)
        );
        assert_eq!(
            index_port_if_eligible(&ws, &[spec(true)]),
            None,
            "catch-all service must suppress index.html"
        );
        let empty = tempfile::tempdir().expect("tempdir").keep();
        assert_eq!(index_port_if_eligible(&empty, &[spec(false)]), None);
    }
}
