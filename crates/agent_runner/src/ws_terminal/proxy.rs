//! 单连接代理：浏览器 WS ↔ 本地 ttyd
//!
//! 每条浏览器 WS 连接对应一条新建的到本地 ttyd 的 WS 连接。cd 由「连接 ttyd 时
//! 注入 `arg=--cwd&arg={项目目录}`」控制——这是代码逻辑，每次连接（含重连）必然执行，
//! 彻底摆脱 Pingora `upstream_request_filter` 对 WS 只首次触发的结构性缺陷。

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use shared_types::{ServiceType, TTYD_PORT};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::{debug, info, trace, warn};

use crate::ws_terminal::cwd;

/// ttyd 的 WebSocket 端点路径
const TTYD_WS_PATH: &str = "/ws";

/// connect_async 失败后的重试次数（不含首次尝试）
const TTYD_CONNECT_RETRIES: u32 = 3;

/// 每次重试间隔
const TTYD_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// 向 browser 方向发送的 keepalive 帧间隔（秒）。
///
/// 终端空闲时 ttyd 是被动协议、无任何下行，会导致上游 java 透明代理
/// (clientIdle, readerIdle=600s) 读空闲超时主动断连。中间层每 30s 主动向 browser
/// 发一帧 keepalive，经 pingora→java 回传，刷新 java 读空闲；同时兜底刷新前端
/// 「无消息」判活（见 EmbeddedConsoleTerminal，已移除该判活，此处作双保险）。
///
/// **契约（跨仓库）**：此值必须显著小于 java 端 `ComputerProxyServerContainer.
/// readTimeoutSeconds`(=600s)，否则 keepalive 来不及刷新 readerIdle、静默失效。
/// 若调整 java 该值，须同步核对此处。
const KEEPALIVE_INTERVAL_SECS: u64 = 30;

/// keepalive 帧载荷：单字节 0x90。
///
/// 选 0x90 因 ttyd 协议未使用该 cmd（OUTPUT=0x30），前端 decodeTtydMessage 对非
/// 0x30 帧返回空串、不写入 xterm，对终端显示零副作用。
const KEEPALIVE_PAYLOAD: &[u8] = &[0x90];

/// 处理一条「浏览器 → ttyd」的代理连接
///
/// 1. 由 serviceType + project_id 解析项目工作目录（白名单校验）
/// 2. `connect_async` 连本地 ttyd（含重试），URL 注入 `arg=--cwd&arg={cwd}`（wrapper 据此 cd）
/// 3. 双向透传 Message（原样，不解析帧语义）
pub async fn handle_terminal(
    browser_ws: WebSocketStream<TcpStream>,
    service_type: &str,
    project_id: &str,
    tenant_id: &str,
    space_id: &str,
) {
    let cwd = cwd::resolve_project_cwd(service_type, project_id, tenant_id, space_id);

    // cwd 解析失败的降级策略（按 service_type 区分）：
    // - WebAgentRunner：项目目录必须在 /app/project_workspace 下，进 /home/user 毫无意义。
    //   解析失败（目录不存在 / tenant-space 反查缺失）→ **fail-closed**，关连接并明确告知
    //   前端"项目目录不可用"，避免误导用户进入空白家目录。
    // - ComputerAgentRunner / service_type 未知：/home/user 是 user 家目录，可作合理默认
    //   （**fail-open**），落 ttyd 默认目录，至少保留可用 shell。
    match &cwd {
        Some(p) => info!(
            "[WS_TERMINAL] connecting ttyd: service_type={}, project_id={}, tenant_id={}, space_id={}, cwd={}",
            service_type, project_id, tenant_id, space_id, p.display()
        ),
        None => {
            let is_web = matches!(
                ServiceType::from_str(service_type),
                Ok(ServiceType::WebAgentRunner)
            );
            if is_web {
                warn!(
                    "[WS_TERMINAL] project workspace not found, closing (fail-closed): service_type={}, project_id={}, tenant_id={}, space_id={}",
                    service_type, project_id, tenant_id, space_id
                );
                close_with_reason(
                    browser_ws,
                    CloseCode::Error,
                    "Project workspace not found, please retry",
                )
                .await;
                return;
            }
            warn!(
                "[WS_TERMINAL] cwd resolved None, falling back to ttyd default home: service_type={}, project_id={}",
                service_type, project_id
            );
        }
    }

    let url = build_ttyd_url(cwd.as_deref());

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

    // b(ttyd) → a(browser)，并周期注入 keepalive
    //
    // 终端空闲时 ttyd 无任何下行，会带来两个问题：
    //   1. 前端若以「无消息」判活会超时（已在 EmbeddedConsoleTerminal 移除，留作兜底）
    //   2. 上游 java 透明代理 clientIdle(readerIdle=600s) 读空闲 → 主动断连
    // 每 KEEPALIVE_INTERVAL_SECS 向 browser 方向发一帧 keepalive：字节 0x90 非
    // OUTPUT(0x30)，前端 decodeTtydMessage 返回空串、不写入 xterm（零副作用）；该帧
    // 经 pingora→java→browser 回传，刷新 java clientIdle 读空闲，避免空闲 600s 被断。
    //
    // 契约：此帧仅对 ttyd wireProtocol 客户端安全。ws_terminal 只服务 ttyd 终端链路
    // （协商 'tty' 子协议、转发本地 ttyd），上游所有连接前端必为 wireProtocol='ttyd'
    // （见 nuwax TTYD_TERMINAL_WIRE_PROTOCOL），故 0x90 必被前端忽略、不会污染显示。
    let ba = async {
        let mut ticker = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let _ = ticker.tick().await; // 丢弃首次立即触发（连接刚建立，无需立刻保活）
        let mut first_keepalive = true; // 首次 info 确认保活已生效，其后 trace 避免刷屏
        loop {
            tokio::select! {
                msg = b_src.next() => match msg {
                    Some(Ok(m)) => {
                        if a_sink.send(m).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                },
                _ = ticker.tick() => {
                    if a_sink
                        .send(Message::Binary(Bytes::from_static(KEEPALIVE_PAYLOAD)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if first_keepalive {
                        info!(
                            "[WS_TERMINAL] keepalive active (interval={}s)",
                            KEEPALIVE_INTERVAL_SECS
                        );
                        first_keepalive = false;
                    } else {
                        trace!("[WS_TERMINAL] keepalive sent");
                    }
                }
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

    /// 集成测试：relay 必须把 ttyd 真实输出转发到 browser，并在空闲时注入 keepalive。
    ///
    /// 两条本地 loopback WS 连接模拟「browser ↔ relay ↔ ttyd」：a_client 给 relay 当
    /// browser、b_client 当 ttyd；各自对端 a_server/b_server 由测试控制，分别读取转发
    /// 结果与注入 ttyd 输出。
    ///
    /// 时间控制：不能用 `start_paused`（会让转发步骤的真实 IO 被虚拟 timeout 抢先误判）。
    /// 改为「转发用真实时间，仅 keepalive 阶段 `pause`+`advance` 快进」，既稳定又无需
    /// 真实等待 30s。
    #[tokio::test]
    async fn relay_forwards_ttyd_data_and_injects_keepalive_when_idle() {
        use futures_util::StreamExt;
        use tokio::net::TcpListener;

        // 建立一对 loopback WS 连接：(client 连到 listener, listener accept 出的 server)
        async fn mk_pair() -> (
            WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
            WebSocketStream<TcpStream>,
        ) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (s, _) = listener.accept().await.unwrap();
                tokio_tungstenite::accept_async(s).await.unwrap()
            });
            let (client, _) =
                tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
            (client, server.await.unwrap())
        }

        // a=browser（a_server 读 keepalive + 转发数据）；b=ttyd（b_server 模拟 ttyd 输出）
        let (a_client, mut a_server) = mk_pair().await;
        let (b_client, mut b_server) = mk_pair().await;

        // 模拟 ttyd 真实输出
        b_server
            .send(Message::Binary(Bytes::copy_from_slice(b"real-data")))
            .await
            .unwrap();

        let relay = tokio::spawn(relay(a_client, b_client));

        // 1. ttyd 数据应被原样透传到 browser（真实时间：数据已在 buffer，宽松 timeout 兜底）
        let forwarded = tokio::time::timeout(Duration::from_secs(10), a_server.next())
            .await
            .expect("转发 ttyd 数据超时")
            .expect("a_server 流提前关闭");
        match forwarded {
            Ok(Message::Binary(d)) => assert_eq!(d.as_ref(), b"real-data"),
            other => panic!("期望 Binary 转发数据，实际 {other:?}"),
        }

        // 2. 空闲后应收到 keepalive：暂停时钟并快进到 ticker 触发之后
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(KEEPALIVE_INTERVAL_SECS + 5)).await;
        let keepalive = tokio::time::timeout(Duration::from_secs(10), a_server.next())
            .await
            .expect("keepalive 帧超时未到达")
            .expect("a_server 流提前关闭");
        match keepalive {
            Ok(Message::Binary(d)) => assert_eq!(d.as_ref(), KEEPALIVE_PAYLOAD),
            other => panic!("期望 keepalive Binary，实际 {other:?}"),
        }
        tokio::time::resume();

        // 3. 对端全部关闭后 relay 应能自行结束（验证不泄漏 task）
        drop(a_server);
        drop(b_server);
        tokio::time::timeout(Duration::from_secs(10), relay)
            .await
            .expect("relay 未在超时内结束（疑似 task 泄漏）")
            .expect("relay task panic");
    }
}
