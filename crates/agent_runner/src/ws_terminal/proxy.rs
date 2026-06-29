//! 单连接代理：浏览器 WS ↔ 本地 ttyd
//!
//! 每条浏览器 WS 连接对应一条新建的到本地 ttyd 的 WS 连接。cd 由「连接 ttyd 时
//! 注入 `arg=--cwd&arg={项目目录}`」控制——这是代码逻辑，每次连接（含重连）必然执行，
//! 彻底摆脱 Pingora `upstream_request_filter` 对 WS 只首次触发的结构性缺陷。

use std::path::Path;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use shared_types::TTYD_PORT;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::{debug, info, warn};

use crate::ws_terminal::cwd;

/// ttyd 的 WebSocket 端点路径
const TTYD_WS_PATH: &str = "/ws";

/// connect_async 失败后的重试次数（不含首次尝试）
const TTYD_CONNECT_RETRIES: u32 = 3;

/// 每次重试间隔
const TTYD_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// 处理一条「浏览器 → ttyd」的代理连接
///
/// 1. 由 serviceType + project_id 解析项目工作目录（白名单校验）
/// 2. `connect_async` 连本地 ttyd（含重试），URL 注入 `arg=--cwd&arg={cwd}`（wrapper 据此 cd）
/// 3. 双向透传 Message（原样，不解析帧语义）
pub async fn handle_terminal(
    browser_ws: WebSocketStream<TcpStream>,
    service_type: &str,
    project_id: &str,
) {
    let cwd = cwd::resolve_project_cwd(service_type, project_id);
    let url = build_ttyd_url(cwd.as_deref());
    info!(
        "[WS_TERMINAL] connecting ttyd {} (service_type={}, project_id={}, cwd={:?})",
        url, service_type, project_id, cwd
    );

    // ttyd（libwebsockets）要求 WS 握手带子协议 `tty`，否则路由到 http-only 空壳、消息全丢。
    // 浏览器侧由 ws_terminal 协商 tty；agent_runner 作为 client 连 ttyd 也必须显式带上。
    let mut req = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "[WS_TERMINAL] build ttyd request failed: {} (service_type={}, project_id={})",
                e, service_type, project_id
            );
            close_with_reason(
                browser_ws,
                CloseCode::Error,
                "Terminal backend request build failed",
            )
            .await;
            return;
        }
    };
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        axum::http::HeaderValue::from_static("tty"),
    );

    // 带重试的连接：ttyd 刚启动时 TCP 端口可能通了但 WS 握手未就绪，
    // 重试几次覆盖这种启动竞态，避免前端首次连接失败。
    let ttyd_ws = match connect_ttyd_with_retry(req, service_type, project_id).await {
        Ok(ws) => ws,
        Err(last_err) => {
            // All retries exhausted — close browser WS with English reason so frontend can distinguish
            warn!(
                "[WS_TERMINAL] all ttyd connect attempts exhausted: {} (service_type={}, project_id={})",
                last_err, service_type, project_id
            );
            close_with_reason(
                browser_ws,
                CloseCode::Error,
                "Terminal backend not ready, please retry",
            )
            .await;
            return;
        }
    };

    info!(
        "[WS_TERMINAL] relay started (service_type={}, project_id={})",
        service_type, project_id
    );
    relay(browser_ws, ttyd_ws).await;
    info!(
        "[WS_TERMINAL] relay ended (service_type={}, project_id={})",
        service_type, project_id
    );
}

/// 带重试地连接 ttyd WebSocket
///
/// 首次失败后最多重试 `TTYD_CONNECT_RETRIES` 次，每次间隔 `TTYD_RETRY_INTERVAL`。
/// 每次失败时记录详细日志（区分 TCP 不可达 vs WS 握手失败）。
async fn connect_ttyd_with_retry(
    req: tokio_tungstenite::tungstenite::handshake::client::Request,
    service_type: &str,
    project_id: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>, String>
{
    let max_attempts = TTYD_CONNECT_RETRIES + 1; // initial + retries
    let mut last_err = String::from("no connection attempts made");

    for attempt in 1..=max_attempts {
        // TCP pre-check: distinguish "port not open" from "port open but WS handshake failed"
        let tcp_ok = is_tcp_port_open(TTYD_PORT).await;

        if !tcp_ok {
            last_err = format!("ttyd port {} not reachable", TTYD_PORT);
            warn!(
                "[WS_TERMINAL] ttyd port {} not reachable, attempt {}/{} (service_type={}, project_id={})",
                TTYD_PORT, attempt, max_attempts, service_type, project_id
            );
        } else {
            debug!(
                "[WS_TERMINAL] ttyd port {} reachable, attempting WS connect {}/{}",
                TTYD_PORT, attempt, max_attempts
            );
            let req_clone = req.clone();
            match connect_async(req_clone).await {
                Ok((ws, _resp)) => {
                    if attempt > 1 {
                        info!(
                            "[WS_TERMINAL] ttyd WS connected after {} attempts (service_type={}, project_id={})",
                            attempt, service_type, project_id
                        );
                    }
                    return Ok(ws);
                }
                Err(e) => {
                    last_err = e.to_string();
                    warn!(
                        "[WS_TERMINAL] connect ttyd WS failed, attempt {}/{}: {} (service_type={}, project_id={})",
                        attempt, max_attempts, last_err, service_type, project_id
                    );
                }
            }
        }

        if attempt < max_attempts {
            tokio::time::sleep(TTYD_RETRY_INTERVAL).await;
        }
    }

    Err(last_err)
}

/// 检测本地 TCP 端口是否可达（快速 timeout，不阻塞）
async fn is_tcp_port_open(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_millis(300),
        tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

/// 构造连 ttyd 的 URL，注入 `--cwd`（与 Pingora 旧逻辑等价，但由本模块每次连接执行）
fn build_ttyd_url(cwd: Option<&Path>) -> String {
    let base = format!("ws://127.0.0.1:{}{}", TTYD_PORT, TTYD_WS_PATH);
    match cwd {
        Some(p) => format!("{}?arg=--cwd&arg={}", base, p.display()),
        None => base,
    }
}

/// 双向透传：浏览器 ↔ ttyd
///
/// 并发等双向结束：一方读完后主动给对方发 Close frame，触发对方也结束（链式优雅关闭）。
/// 相比 `select!`「任一结束就 drop 另一方」，这里双方都能发出 WS Close frame，
/// 前端收到正常关闭(1000)而非异常(1006)。
async fn relay<S1, S2>(a: WebSocketStream<S1>, b: WebSocketStream<S2>)
where
    S1: AsyncRead + AsyncWrite + Unpin,
    S2: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_sink, mut a_src) = a.split();
    let (mut b_sink, mut b_src) = b.split();

    // a(browser) → b(ttyd)
    let ab = async {
        while let Some(msg) = a_src.next().await {
            match msg {
                Ok(m) => {
                    if b_sink.send(m).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = b_sink.close().await;
    };

    // b(ttyd) → a(browser)
    let ba = async {
        while let Some(msg) = b_src.next().await {
            match msg {
                Ok(m) => {
                    if a_sink.send(m).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = a_sink.close().await;
    };

    tokio::join!(ab, ba);
    debug!("[WS_TERMINAL] relay closed (both directions ended)");
}

/// Close a WS with a reason code and English message (replaces silent close).
///
/// Sends a Close frame so the frontend can distinguish "backend not ready" from
/// a generic abnormal closure (1006).
async fn close_with_reason<S>(ws: WebSocketStream<S>, code: CloseCode, reason: &str)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, _src) = ws.split();
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    // Send explicit Close frame with reason, then close the sink
    let _ = sink.send(Message::Close(Some(frame))).await;
    let _ = sink.close().await;
    info!(
        "[WS_TERMINAL] connection closed with reason: {} (code={})",
        reason, code
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn build_url_without_cwd_has_no_arg() {
        let url = build_ttyd_url(None);
        assert_eq!(url, "ws://127.0.0.1:7681/ws");
        assert!(!url.contains("arg="));
    }

    #[test]
    fn build_url_with_cwd_injects_arg() {
        let cwd = PathBuf::from("/home/user/proj-1");
        let url = build_ttyd_url(Some(&cwd));
        assert_eq!(
            url,
            "ws://127.0.0.1:7681/ws?arg=--cwd&arg=/home/user/proj-1"
        );
    }
}
