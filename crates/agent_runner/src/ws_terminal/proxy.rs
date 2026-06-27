//! 单连接代理：浏览器 WS ↔ 本地 ttyd
//!
//! 每条浏览器 WS 连接对应一条新建的到本地 ttyd 的 WS 连接。cd 由「连接 ttyd 时
//! 注入 `arg=--cwd&arg={项目目录}`」控制——这是代码逻辑，每次连接（含重连）必然执行，
//! 彻底摆脱 Pingora `upstream_request_filter` 对 WS 只首次触发的结构性缺陷。

use std::path::Path;

use futures_util::{SinkExt, StreamExt};
use shared_types::TTYD_PORT;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, info, warn};

use crate::ws_terminal::cwd;

/// ttyd 的 WebSocket 端点路径
const TTYD_WS_PATH: &str = "/ws";

/// 处理一条「浏览器 → ttyd」的代理连接
///
/// 1. 由 serviceType + project_id 解析项目工作目录（白名单校验）
/// 2. `connect_async` 连本地 ttyd，URL 注入 `arg=--cwd&arg={cwd}`（wrapper 据此 cd）
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
            warn!("[WS_TERMINAL] build ttyd request failed: {}", e);
            close_silently(browser_ws).await;
            return;
        }
    };
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        axum::http::HeaderValue::from_static("tty"),
    );
    let ttyd_ws = match connect_async(req).await {
        Ok((ws, _resp)) => ws,
        Err(e) => {
            warn!("[WS_TERMINAL] connect ttyd failed: {}", e);
            // 关闭浏览器侧连接，让前端感知失败并重试
            close_silently(browser_ws).await;
            return;
        }
    };

    relay(browser_ws, ttyd_ws).await;
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

    // a(浏览器) → b(ttyd)
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

    // b(ttyd) → a(浏览器)
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

    // 并发等双向：一方结束会主动给对方发 Close frame，触发对方也结束 —— 链式优雅关闭。
    // 相比 select!「任一结束就 drop 另一方」，双方都能发出 WS Close frame，
    // 前端收到正常关闭(1000)而非异常(1006)。
    tokio::join!(ab, ba);
    debug!("[WS_TERMINAL] relay closed (both directions ended)");
}

/// 静默关闭一个 WS（用于错误路径兜底，忽略关闭本身的错误）
async fn close_silently<S>(ws: WebSocketStream<S>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, _src) = ws.split();
    let _ = sink.close().await;
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
