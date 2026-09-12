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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use workspace_manifest::ProjectType;

use crate::manifest::ServiceSpec;

static HOSTED: LazyLock<StaticHostManager> = LazyLock::new(StaticHostManager::default);

#[derive(Default)]
pub struct StaticHostManager {
    hosts: Mutex<HashMap<u16, HostedService>>,
}

struct HostedService {
    root: watch::Sender<PathBuf>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    drained: watch::Receiver<bool>,
}

impl Drop for HostedService {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

struct CancelWorkerOnDrop(CancellationToken);
impl Drop for CancelWorkerOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl HostedService {
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.cancel.cancel();
        let wait = async {
            while !*self.drained.borrow_and_update() {
                self.drained.changed().await.map_err(|_| {
                    anyhow::anyhow!("static connection owner exited without confirming drain")
                })?;
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), wait)
            .await
            .map_err(|_| {
                crate::supervisor::ShutdownUnconfirmed(
                    "static connection drain deadline exceeded".into(),
                )
            })?
            .map_err(|error| crate::supervisor::ShutdownUnconfirmed(error.to_string()))?;
        self.task.abort();
        Ok(())
    }
}

/// Own every connection driver. Cancellation of the monitor cannot detach HTTP
/// connections: this worker closes accept, drains briefly, then aborts and joins.
async fn serve_owned_connections(
    listener: tokio::net::TcpListener,
    router: Router,
    cancel: CancellationToken,
    drained: watch::Sender<bool>,
) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((socket, _)) => {
                    let service = hyper_util::service::TowerToHyperService::new(router.clone());
                    let cancelled = cancel.clone();
                    connections.spawn(async move {
                        let connection = hyper::server::conn::http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(socket), service);
                        tokio::pin!(connection);
                        tokio::select! {
                            result = &mut connection => { if let Err(error) = result { tracing::debug!(%error, "Static HTTP connection ended"); } },
                            () = cancelled.cancelled() => {
                                connection.as_mut().graceful_shutdown();
                                drop(connection.await);
                            }
                        }
                    });
                }
                Err(error) => { tracing::error!(%error, "Static accept failed"); cancel.cancel(); break; }
            },
            _ = connections.join_next(), if !connections.is_empty() => {},
        }
    }
    drop(listener);
    if tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    drained.send_replace(true);
}

fn bind_listener(port: u16) -> std::io::Result<std::net::TcpListener> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    // Reclaim a stopped listener's address even while old HTTP sockets remain
    // in TIME_WAIT. This never enables reuse_port or competing live listeners.
    #[cfg(unix)]
    socket.set_reuseaddr(true)?;
    socket
        .bind(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
        .map_err(|error| std::io::Error::new(error.kind(), format!("bind: {error}")))?;
    socket
        .listen(1024)
        .map_err(|error| std::io::Error::new(error.kind(), format!("listen: {error}")))?
        .into_std()
}

impl StaticHostManager {
    /// Stage every new bind before changing active roots. A rejected configuration
    /// leaves serving listeners unchanged. Reuse is based on the actual live port,
    /// allowing services to exchange ports without an unnecessary bind conflict.
    pub async fn reconcile(
        &self,
        specs: &[ServiceSpec],
        workspace: &Path,
        dev_profile: bool,
    ) -> anyhow::Result<()> {
        self.reconcile_with_bind(specs, workspace, dev_profile, bind_listener)
            .await
    }

    async fn reconcile_with_bind(
        &self,
        specs: &[ServiceSpec],
        workspace: &Path,
        dev_profile: bool,
        mut bind: impl FnMut(u16) -> std::io::Result<std::net::TcpListener>,
    ) -> anyhow::Result<()> {
        let mut desired = HashMap::new();
        let mut names = HashSet::new();
        for spec in specs
            .iter()
            .filter(|spec| spec.enabled && hosts_statically(spec, dev_profile))
        {
            anyhow::ensure!(
                names.insert(&spec.service_id),
                "duplicate static service {}",
                spec.service_id
            );
            let content = spec.static_content_dir.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "static service '{}' has no static_content_dir in release lock",
                    spec.service_id
                )
            })?;
            let root = workspace.join(&spec.dir).join(content);
            anyhow::ensure!(
                desired.insert(spec.port, root).is_none(),
                "duplicate static port {}",
                spec.port
            );
        }
        let mut hosts = self.hosts.lock().await;
        // A finished monitor can still have a worker draining connections.
        // Rebinding is allowed only after that owner confirms every socket closed.
        for port in desired.keys() {
            if let Some(host) = hosts.get_mut(port)
                && (host.task.is_finished() || host.cancel.is_cancelled())
            {
                host.stop().await?;
                hosts.remove(port);
            }
        }
        let mut staged = HashMap::new();
        for (&port, root) in &desired {
            if hosts
                .get(&port)
                .is_some_and(|host| !host.task.is_finished())
            {
                continue;
            }
            let listener = bind(port)
                .map_err(|e| anyhow::anyhow!("bind static host 127.0.0.1:{port}: {e}"))?;
            listener.set_nonblocking(true)?;
            staged.insert(port, tokio::net::TcpListener::from_std(listener)?);
            if !root.is_dir() {
                tracing::warn!(path = %root.display(), "Static content directory is missing; serving 404 until created");
            }
        }
        // All requested binds succeeded; publish roots, then retire old owners.
        let removed: Vec<_> = hosts
            .keys()
            .filter(|port| !desired.contains_key(port))
            .copied()
            .collect();
        for (port, root) in desired {
            if let Some(listener) = staged.remove(&port) {
                let (root_tx, root_rx) = watch::channel(root);
                let cancel = CancellationToken::new();
                let (drained_tx, drained) = watch::channel(false);
                let worker = tokio::spawn(serve_owned_connections(
                    listener,
                    router_with_root(root_rx),
                    cancel.clone(),
                    drained_tx,
                ));
                // Capture the guard before first poll so aborting an unpolled
                // monitor still cancels its owned connection worker.
                let guard = CancelWorkerOnDrop(cancel.clone());
                let task = tokio::spawn(async move {
                    let _guard = guard;
                    if let Err(error) = worker.await {
                        tracing::error!(port, %error, "Static connection owner failed");
                    }
                });
                hosts.insert(
                    port,
                    HostedService {
                        root: root_tx,
                        cancel,
                        task,
                        drained,
                    },
                );
            } else if let Some(host) = hosts.get(&port) {
                host.root.send_replace(root);
            }
        }
        // Keep lifecycle ownership registered until drain is confirmed, including
        // cancellation or an incomplete stop. A later reconcile must finish it.
        for port in removed {
            if let Some(host) = hosts.get_mut(&port) {
                host.stop().await?;
            }
            hosts.remove(&port);
        }
        Ok(())
    }
}

pub async fn reconcile(
    specs: &[ServiceSpec],
    workspace: &Path,
    dev_profile: bool,
) -> anyhow::Result<()> {
    HOSTED.reconcile(specs, workspace, dev_profile).await
}

/// static 服务是否应走内置托管（而非 spawn 进程）：
/// `type = static` 且（非 dev 源码形态 或 未配 `[devrun]`）。
/// dev 源码态 + `[devrun]`：端口让给 dev server（vite dev 热加载）。
pub fn hosts_statically(spec: &ServiceSpec, dev_profile: bool) -> bool {
    spec.r#type == ProjectType::Static && !(dev_profile && spec.devrun.is_some())
}

#[cfg(test)]
fn router(root: PathBuf) -> Router {
    let (_tx, rx) = watch::channel(root);
    router_with_root(rx)
}

fn router_with_root(root: watch::Receiver<PathBuf>) -> Router {
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
async fn serve(State(root): State<watch::Receiver<PathBuf>>, method: Method, uri: Uri) -> Response {
    let root = root.borrow().clone();
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

    #[tokio::test]
    async fn hot_reconfigure_changes_actual_static_content() {
        // Keep port-release assertions isolated from unrelated tests spawning
        // process groups. On Unix, CLOEXEC descriptors can survive temporarily
        // between fork and exec in those children, outside this server's owner.
        const ISOLATED: &str = "APP_CLI_STATIC_LIFECYCLE_TEST_CHILD";
        if std::env::var_os(ISOLATED).is_none() {
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "static_hosting::tests::hot_reconfigure_changes_actual_static_content",
                    "--nocapture",
                ])
                .env(ISOLATED, "1")
                .kill_on_drop(true)
                .output();
            let output = tokio::time::timeout(std::time::Duration::from_secs(30), output)
                .await
                .expect("isolated static lifecycle deadline")
                .expect("spawn isolated static lifecycle test");
            assert!(
                output.status.success(),
                "isolated static lifecycle failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().expect("workspace");
        for (dir, content) in [("dist-a", "RELEASE_A"), ("dist-b", "RELEASE_B")] {
            std::fs::create_dir_all(root.path().join("web").join(dir)).expect("dir");
            std::fs::write(
                root.path().join("web").join(dir).join("index.html"),
                content,
            )
            .expect("file");
        }
        let probe = bind_listener(0).expect("port");
        let port = probe.local_addr().expect("address").port();
        let mut probe = Some(probe);
        let mut spec = crate::manifest::ServiceSpec {
            service_id: uuid::Uuid::new_v4().to_string(),
            name: "Web".into(),
            dir: "web".into(),
            r#type: ProjectType::Static,
            kind: workspace_manifest::ProjectKind::Web,
            enabled: true,
            port,
            devbuild: None,
            run: Default::default(),
            devrun: None,
            static_content_dir: Some("dist-a".into()),
            health: Default::default(),
            proxy: None,
            logs: Vec::new(),
            env: Default::default(),
        };
        let manager = StaticHostManager::default();
        manager
            .reconcile_with_bind(
                std::slice::from_ref(&spec),
                root.path(),
                false,
                |requested| {
                    assert_eq!(requested, port);
                    Ok(probe.take().expect("reserved A listener"))
                },
            )
            .await
            .expect("start A");
        let url = format!("http://127.0.0.1:{port}/");
        assert_eq!(
            reqwest::get(&url).await.unwrap().text().await.unwrap(),
            "RELEASE_A"
        );
        spec.static_content_dir = Some("dist-b".into());
        manager
            .reconcile(std::slice::from_ref(&spec), root.path(), false)
            .await
            .expect("switch B");
        assert_eq!(
            reqwest::get(&url).await.unwrap().text().await.unwrap(),
            "RELEASE_B"
        );
        let accepted = spec.clone();
        let occupied = bind_listener(0).expect("occupied port");
        spec.port = occupied.local_addr().expect("occupied address").port();
        spec.static_content_dir = Some("dist-a".into());
        assert!(
            manager
                .reconcile(std::slice::from_ref(&spec), root.path(), false)
                .await
                .is_err()
        );
        assert_eq!(
            reqwest::get(&url).await.unwrap().text().await.unwrap(),
            "RELEASE_B",
            "rejected bind must preserve serving root"
        );
        let mut reserved = Some(occupied);
        manager
            .reconcile_with_bind(
                std::slice::from_ref(&spec),
                root.path(),
                false,
                |requested| {
                    assert_eq!(requested, spec.port);
                    Ok(reserved.take().expect("reserved replacement listener"))
                },
            )
            .await
            .unwrap();
        let changed_url = format!("http://127.0.0.1:{}/", spec.port);
        assert_eq!(
            reqwest::get(&changed_url)
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "RELEASE_A"
        );
        let mut previous = Some(bind_listener(port).expect("old port released"));
        manager
            .reconcile_with_bind(
                std::slice::from_ref(&accepted),
                root.path(),
                false,
                |requested| {
                    assert_eq!(requested, port);
                    Ok(previous.take().expect("reserved previous listener"))
                },
            )
            .await
            .unwrap();
        assert_eq!(
            reqwest::get(&url).await.unwrap().text().await.unwrap(),
            "RELEASE_B",
            "recovery restores accepted root and port"
        );
        assert!(
            bind_listener(spec.port).is_ok(),
            "recovery releases rejected generation port"
        );
        // A completed response proves this persistent connection is owned by
        // the server before its monitor is aborted. Leave the next request partial.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut held = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        held.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !response
                .windows(b"RELEASE_B".len())
                .any(|chunk| chunk == b"RELEASE_B")
            {
                let mut chunk = [0u8; 1024];
                let length = held.read(&mut chunk).await.unwrap();
                assert!(length > 0, "response ended before its expected body");
                response.extend_from_slice(&chunk[..length]);
            }
        })
        .await
        .unwrap();
        held.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Stall:")
            .await
            .unwrap();
        {
            let mut hosts = manager.hosts.lock().await;
            let host = hosts.get_mut(&port).unwrap();
            host.task.abort();
            let _ = (&mut host.task).await;
        }
        manager
            .reconcile(std::slice::from_ref(&accepted), root.path(), false)
            .await
            .unwrap();

        assert_eq!(
            reqwest::get(&url).await.unwrap().text().await.unwrap(),
            "RELEASE_B",
            "exited listener must be restarted"
        );
        let mut trailing = Vec::new();
        let closed = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            held.read_to_end(&mut trailing),
        )
        .await
        .expect("old connection must reach EOF before rebind completes");
        if let Err(error) = closed {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ),
                "unexpected old connection error: {error}"
            );
        }
        manager.reconcile(&[], root.path(), false).await.unwrap();
        assert!(
            bind_listener(port).is_ok(),
            "removed static service releases port"
        );
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
