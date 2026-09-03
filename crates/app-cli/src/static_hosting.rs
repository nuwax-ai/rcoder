//! `type = static` 服务的内置静态托管。
//!
//! 无进程：bind `127.0.0.1:{lock 分配端口}` 递归 serve
//! `{workspace}/{dir}/{[build].artifact 目录}`——pingap upstream/路由/启动事件
//! 与进程态服务**完全同构**（upstream 就是 127.0.0.1:port）。`GET /health`
//! 固定 200（探针/启动判定）；未匹配路径 SPA fallback 到 `index.html`
//!（前端 client routing，对齐退役的 static-server.cjs 语义）。
//!
//! 与 `[devrun]` 正交：dev 源码态（`APP_CLI_RUN_PROFILE=dev`）且配置了
//! `[devrun]` 时**端口让给 dev server**（vite dev 热加载占同端口），托管跳过
//! ——由启动循环分派。幂等：热部署重编排（builtin Redeploy / supervisord 重
//! orchestrate）不二次 bind 同端口。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use workspace_manifest::ProjectType;

use crate::manifest::ServiceSpec;

/// 已托管服务集合（service_id → listener 已起；幂等防热部署重编排二次 bind）。
static HOSTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// static 服务是否应走内置托管（而非 spawn 进程）：
/// `type = static` 且（非 dev 源码形态 或 未配 `[devrun]`）。
/// dev 源码态 + `[devrun]`：端口让给 dev server（vite dev 热加载）。
pub fn hosts_statically(spec: &ServiceSpec, dev_profile: bool) -> bool {
    spec.r#type == ProjectType::Static && !(dev_profile && spec.devrun.is_some())
}

/// 启动某 static 服务的托管 listener（幂等：已在托管则 Ok）。bind 失败回滚
/// 标记并上抛（端口被外部占用 = 编排 fail-fast）。
pub fn ensure_spawned(spec: &ServiceSpec, workspace: &Path) -> anyhow::Result<()> {
    {
        let mut guard = HOSTED.lock().expect("static hosted set lock");
        let hosted = guard.get_or_insert_with(HashSet::new);
        if hosted.contains(&spec.service_id) {
            return Ok(());
        }
        hosted.insert(spec.service_id.clone());
    }
    let spawn_result = spawn_listener(spec, workspace);
    if spawn_result.is_err() {
        HOSTED
            .lock()
            .expect("static hosted set lock")
            .get_or_insert_with(HashSet::new)
            .remove(&spec.service_id);
    }
    spawn_result
}

fn spawn_listener(spec: &ServiceSpec, workspace: &Path) -> anyhow::Result<()> {
    // 托管内容根：{workspace}/{dir}/{static_content_dir}（lock 透传的
    // [build].artifact 目录；hosts_statically 已保证 static 服务必达）
    let Some(content_dir) = &spec.static_content_dir else {
        anyhow::bail!(
            "static service '{}' has no static_content_dir in release lock",
            spec.service_id
        )
    };
    let root = workspace.join(&spec.dir).join(content_dir);
    // 产物缺失可见性：root 不存在（如源码态未跑过构建）不阻断 bind（后续构建
    // 出目录即自动生效——每请求实时读），但必须 warn 留痕（否则 404 无迹可循）
    if !root.is_dir() {
        tracing::warn!(
            "static host '{}' content dir missing (serving 404 until it appears): {}",
            spec.service_id,
            root.display()
        );
    }
    let listener = std::net::TcpListener::bind(("127.0.0.1", spec.port))
        .map_err(|e| anyhow::anyhow!("bind static host 127.0.0.1:{}: {e}", spec.port))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("static host listener nonblocking: {e}"))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| anyhow::anyhow!("static host listener async: {e}"))?;
    let router = router(root);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!("static host exited: {e}");
        }
    });
    Ok(())
}

fn router(root: PathBuf) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/{*path}", get(serve).head(serve))
        .route("/", get(serve).head(serve))
        .with_state(root)
}

/// 探针端点：固定 200（托管恒活——与启动判定 service_start_ok 语义一致）。
async fn health() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// 递归 serve 静态目录；未匹配路径 SPA fallback 到 index.html；HEAD 剥 body。
async fn serve(State(root): State<PathBuf>, method: Method, uri: Uri) -> Response {
    let Some(target) = resolve_target(&root, uri.path()) else {
        return not_found();
    };
    match tokio::fs::read(&target).await {
        Ok(bytes) => {
            let mut headers = HeaderMap::new();
            if let Some(mime) = content_type(&target) {
                headers.insert(
                    header::CONTENT_TYPE,
                    mime.parse().unwrap_or_else(|_| {
                        "application/octet-stream".parse().expect("static mime")
                    }),
                );
            }
            if method == Method::HEAD {
                headers.insert(
                    header::CONTENT_LENGTH,
                    bytes.len().to_string().parse().expect("decimal length"),
                );
                return (StatusCode::OK, headers, axum::body::Body::empty()).into_response();
            }
            (StatusCode::OK, headers, bytes).into_response()
        }
        // 未匹配（含目录）：SPA fallback——index.html 由前端路由接管
        //（对齐 static-server.cjs 语义；无 index.html 时 404）
        Err(_) => match tokio::fs::read(root.join("index.html")).await {
            Ok(bytes) => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::CONTENT_TYPE,
                    "text/html; charset=utf-8"
                        .parse()
                        .expect("static html mime"),
                );
                if method == Method::HEAD {
                    headers.insert(
                        header::CONTENT_LENGTH,
                        bytes.len().to_string().parse().expect("decimal length"),
                    );
                    return (StatusCode::OK, headers, axum::body::Body::empty()).into_response();
                }
                (StatusCode::OK, headers, bytes).into_response()
            }
            Err(_) => not_found(),
        },
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// 请求路径 → 静态目录内目标文件（防穿越：解码后组件必须全 Normal）。
fn resolve_target(root: &Path, request_path: &str) -> Option<PathBuf> {
    if request_path.contains("..") {
        return None;
    }
    let decoded = percent_decode(request_path);
    let mut rel = PathBuf::new();
    for component in Path::new(&decoded).components() {
        match component {
            Component::Normal(seg) => rel.push(seg),
            Component::RootDir | Component::CurDir => continue,
            Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    // 空路径（"/"）→ index.html
    if rel.as_os_str().is_empty() {
        rel.push("index.html");
    }
    Some(root.join(rel))
}

/// 百分号解码（%20 等）；非法序列原样保留（后续读取 404/fallback 兜底）。
fn percent_decode(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
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

fn content_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
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
        "map" => "application/json",
        _ => return None,
    })
}

use std::path::Component;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn temp_root() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.keep();
        std::fs::write(path.join("index.html"), "<html>spa</html>").expect("index");
        std::fs::create_dir_all(path.join("assets")).expect("assets");
        std::fs::write(path.join("assets/app.js"), "console.log(1)").expect("js");
        path
    }

    async fn get(router: &Router, path: &str) -> (StatusCode, String, String) {
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
            .to_bytes();
        (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    /// 根路径/嵌套文件/SPA fallback/health/穿越 全语义。
    #[tokio::test]
    async fn static_host_serves_files_with_spa_fallback() {
        let router = router(temp_root());
        // 根 → index.html
        let (status, content_type, body) = get(&router, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/html"));
        assert_eq!(body, "<html>spa</html>");
        // 嵌套文件（递归 + Content-Type）
        let (status, content_type, body) = get(&router, "/assets/app.js").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "text/javascript; charset=utf-8");
        assert_eq!(body, "console.log(1)");
        // 未匹配路径 → SPA fallback index.html（前端路由接管）
        let (status, content_type, body) = get(&router, "/some/client/route").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/html"));
        assert_eq!(body, "<html>spa</html>");
        // /health 固定 200
        let (status, _, body) = get(&router, "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
        // 穿越拒绝
        let (status, _, _) = get(&router, "/..%2f..%2fetc%2fpasswd").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// hosts_statically 分派：static 且非 devrun-dev → 托管；
    /// dev 源码态 + devrun → 让位 dev server；非 static 恒 false。
    #[test]
    fn hosts_statically_dispatch() {
        let mut spec = crate::manifest::ServiceSpec {
            service_id: "web".into(),
            name: "Web".into(),
            dir: "web".into(),
            r#type: ProjectType::Static,
            kind: workspace_manifest::ProjectKind::Web,
            enabled: true,
            port: 4578,
            devbuild: None,
            run: Default::default(),
            devrun: None,
            static_content_dir: Some("dist".into()),
            health: Default::default(),
            proxy: None,
            logs: Vec::new(),
            env: Default::default(),
        };
        assert!(hosts_statically(&spec, false));
        assert!(hosts_statically(&spec, true), "无 devrun 的 static 恒托管");
        spec.devrun = Some(workspace_manifest::DevrunSection {
            command: vec!["pnpm".into(), "exec".into(), "vite".into()],
        });
        assert!(
            !hosts_statically(&spec, true),
            "dev 源码态 + devrun：端口让给 dev server"
        );
        assert!(hosts_statically(&spec, false), "产物态 + devrun 仍托管");
        spec.r#type = ProjectType::Node;
        assert!(!hosts_statically(&spec, false));
    }
}
