//! WS 终端中间层服务端：监听 + 子协议握手 + 分发
//!
//! 监听对外端口（computer 场景 17681，由 Pingora TtydProxy/WebTtydProxy 路由到此），
//! 用 `accept_hdr_async` 自定义握手：
//! - 从请求头读 Pingora 注入的 `X-Ttyd-Project-Id`，交给 proxy 用于 cd
//! - 协商子协议 `tty`（前端 `new WebSocket(url, ['tty'])` 依赖，不能改前端）
//!
//! cd 由 `cwd::resolve_project_cwd` 自动检测 `/home/user` 与 `/app/project_workspace`
//! 两前缀适配 computer/web 场景，本模块无需感知。
//!
//! 参考实现：`tokio-tungstenite/examples/server.rs` 的 `TcpListener + accept_hdr_async`。

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::http::HeaderValue;
use axum::http::header::SEC_WEBSOCKET_PROTOCOL;
use shared_types::{TTYD_PORT, WS_TERMINAL_PORT};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tracing::{error, info, warn};

use crate::ws_terminal::proxy;

/// ttyd 子协议名（前端固定传 `'tty'`）
const TTYD_SUBPROTO: &str = "tty";

/// Pingora 注入的 project_id 请求头（见 rcoder-proxy `ttyd.rs` 的 `X-Ttyd-Project-Id`）
const PROJECT_ID_HEADER: &str = "x-ttyd-project-id";
const SERVICE_TYPE_HEADER: &str = "x-ttyd-service-type";
const TENANT_ID_HEADER: &str = "x-ttyd-tenant-id";
const SPACE_ID_HEADER: &str = "x-ttyd-space-id";

/// 启动 WS 终端中间层
///
/// - 等 ttyd 内部端口（`TTYD_INTERNAL_PORT`）就绪
/// - 绑定 `addr`（computer 场景 `0.0.0.0:17681`），accept 循环
/// - 每条连接交给 `handle_conn`：握手时取 project_id → 透传给 proxy
///
/// 该函数会一直运行（accept 循环），应在 `main.rs` 用 `tokio::spawn` 并行启动。
pub async fn start_ws_terminal() {
    let addr = format!("0.0.0.0:{}", WS_TERMINAL_PORT);
    if !wait_for_port(TTYD_PORT, 500, 30).await {
        error!(
            "[WS_TERMINAL] ttyd not ready on port {} after retries, abort",
            TTYD_PORT
        );
        return;
    }
    info!(
        "[WS_TERMINAL] ttyd ready on {}, starting listener on {}",
        TTYD_PORT, addr
    );

    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            info!("[WS_TERMINAL] listening on {}", addr);
            l
        }
        Err(e) => {
            error!("[WS_TERMINAL] bind {} failed: {}", addr, e);
            return;
        }
    };

    loop {
        // 进程被 SIGTERM 终止时 accept 返回错误 → 循环退出（容器停机的正常路径）
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("[WS_TERMINAL] accept failed: {}, stopping listener", e);
                break;
            }
        };
        tokio::spawn(async move {
            handle_conn(stream, peer).await;
        });
    }
}

/// 单条 TCP 连接：握手（取 project_id + 协商子协议）→ 交 proxy
async fn handle_conn(stream: tokio::net::TcpStream, peer: SocketAddr) {
    // 握手 callback 只执行一次，使用 OnceLock 把不可变元数据传出，无需四把 Mutex。
    let metadata_slot = Arc::new(OnceLock::new());
    let cb = HandshakeCallback {
        metadata: Arc::clone(&metadata_slot),
    };

    let ws = match accept_hdr_async(stream, cb).await {
        Ok(ws) => ws,
        Err(e) => {
            warn!("[WS_TERMINAL] handshake from {} failed: {}", peer, e);
            return;
        }
    };

    let metadata = metadata_slot.get().cloned().unwrap_or_default();
    info!(
        "[WS_TERMINAL] {} connected (service_type={}, project_id={}, tenant_id={}, space_id={})",
        peer, metadata.service_type, metadata.project_id, metadata.tenant_id, metadata.space_id
    );

    // 计入活跃终端连接：覆盖整个 handle_terminal（含其全部 return 路径），
    // 函数返回时 guard drop → 计数 -1。GetContainerStatus 据此判定容器在用、拦下空闲清理。
    let _conn_guard = super::TerminalConnGuard::new();
    proxy::handle_terminal(
        ws,
        &metadata.service_type,
        &metadata.project_id,
        &metadata.tenant_id,
        &metadata.space_id,
    )
    .await;
}

/// 轮询等待本地端口可达（ttyd readiness 检查）
async fn wait_for_port(port: u16, per_try_ms: u64, retries: u32) -> bool {
    for _ in 0..retries {
        let r = tokio::time::timeout(
            Duration::from_millis(per_try_ms),
            tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)),
        )
        .await;
        if matches!(r, Ok(Ok(_))) {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

#[derive(Clone, Default)]
struct HandshakeMetadata {
    project_id: String,
    service_type: String,
    tenant_id: String,
    space_id: String,
}

/// 握手回调：读 project_id + service_type header + 协商 `tty` 子协议
struct HandshakeCallback {
    metadata: Arc<OnceLock<HandshakeMetadata>>,
}

impl Callback for HandshakeCallback {
    fn on_request(self, req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
        // 1. 读 project_id + service_type（Pingora 注入的 X-Ttyd-Project-Id / X-Ttyd-Service-Type）
        let header = |name| {
            req.headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let _ = self.metadata.set(HandshakeMetadata {
            project_id: header(PROJECT_ID_HEADER),
            service_type: header(SERVICE_TYPE_HEADER),
            tenant_id: header(TENANT_ID_HEADER),
            space_id: header(SPACE_ID_HEADER),
        });

        // 2. 协商子协议：客户端请求含 `tty` 时回选 `tty`，否则不动（保持默认行为）
        let wants_tty = req
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').any(|p| p.trim() == TTYD_SUBPROTO))
            .unwrap_or(false);
        if wants_tty {
            resp.headers_mut().insert(
                SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static(TTYD_SUBPROTO),
            );
        }

        Ok(resp)
    }
}
