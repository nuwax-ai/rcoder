//! UserApp 等不到上游最终响应头的阶段预算回归（specs/userapp-response-
//! header-timeout-20261030）。
//!
//! 生产症状：请求已发往上游（pingap 9080），始终收不到最终响应头——外层
//! OpenResty 约 10 分钟才断开，浏览器长期白屏。修复：外层 RCoder Pingora
//! 仅对 UserApp app dev/prod 两类路由启用"等待上游最终响应头"阶段预算
//! （默认 60s，[`PingoraProxyService::DEFAULT_USERAPP_RESPONSE_HEADER_TIMEOUT`]，
//! 依赖补丁 `PeerOptions::response_header_timeout`）：
//!
//! - 绝对单调 deadline：请求 EOS（含 body）发完后起算，零碎头字节/100/103
//!   不重新起表；合法 101 即止；最终头到达即取消 timer——SSE 已回头、WS
//!   已升级、单次尝试内的慢上传、下载正文不受影响；
//! - 超时只终结该上游交换（H1 连接不回收复用；H2 只结束本流），错误带
//!   独立身份 `ResponseHeaderTimedout`（与普通 ReadTimedout 区分）；
//! - 超时映射 504 + 既有本地错误页管线（浏览器 HTML / API JSON），文案
//!   不宣称应用已停止，超时档不自动刷新（避免自动重复触发业务，特别是
//!   结果可能仍在处理中的写请求）；
//! - 重试受**重试准入墙钟**约束（首次选 peer 起 60s 后不再准入新尝试；
//!   已获准尝试完成上传后等待其分配到的剩余首头预算——整次请求不保证
//!   严格 60s 结束，慢上传会消耗后续重试资格），POST 不自动重放。
//!
//! 全部用例经真实 `PingoraServerManager`（`ProxyServiceWrapper` →
//! `PortProxy`）与真实 dev/prod app 代理路由。压缩用例用测试槽
//! （`set_userapp_response_header_timeout_for_tests`）受控验证边界分支；
//! 核心反例用生产默认 60s 真实等待一轮。

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// prod 测试 wake control（运行态，不触发唤醒等待）。
struct RunningWakeControl;

#[async_trait::async_trait]
impl shared_types::AppWakeControl for RunningWakeControl {
    fn is_stopped(&self, _app_id: &str) -> bool {
        false
    }

    async fn ensure_running(&self, _app_id: &str) -> shared_types::WakeOutcome {
        shared_types::WakeOutcome::AlreadyRunning
    }

    async fn remote_wake_pending(&self, _app_id: &str) -> bool {
        false
    }

    async fn remote_wake_state_fresh(&self, _app_id: &str) -> shared_types::RemoteWakeState {
        shared_types::RemoteWakeState::Running
    }

    fn wake_timeout(&self) -> Duration {
        Duration::from_secs(4)
    }
}

/// 后端读到完整请求头（及声明的 body）后返回的观测。
#[derive(Default)]
struct BackendObs {
    /// 收到完整请求（头+body）的连接数。
    completed: AtomicUsize,
    /// 收到的连接总数（重试次数断言用）。
    accepted: AtomicUsize,
    /// Dribble1xx 形态下成功写出的滴字节数（负载无关的 deadline 语义信号）。
    dribble_bytes: AtomicUsize,
}

/// 读到响应头结束（\r\n\r\n）为止的原始字节。
async fn read_header_bytes(stream: &mut TcpStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        assert_eq!(
            stream.read(&mut byte).await.expect("read header byte"),
            1,
            "connection closed before header end"
        );
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    buffer
}

/// 预留并释放一个本机代理端口。
///
/// 不用 `bind(:0)`：内核临时端口段（macOS 49152+）正是全套件下其它测试
/// `bind(:0)` 与外联连接的分配来源，释放→代理绑定之间的窗口会被并发监听
/// 者抢占（满载全量跑实测：请求落到别人的代理，收到与 fixture 无关的
/// 响应）。改为在临时段以下随机取显式端口——内核不会自动分配该区间，
/// 只有显式绑定者会触碰；同套件内仅本文件 8 个用例使用，随机起点下撞
/// 率可忽略，撞了（bind 失败）换一个重试。
fn reserve_port() -> u16 {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64
        | 1;
    for _ in 0..16 {
        // xorshift64 → 显式端口候选（避开已知服务段，< 49152）
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let port = 25_000 + (seed % 23_000) as u16;
        let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) else {
            continue;
        };
        drop(listener);
        return port;
    }
    panic!("no free explicit proxy port below the ephemeral range");
}

/// 等待代理监听就绪。
async fn wait_listening(port: u16) {
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("isolated proxy never listened on {port}");
}

/// prod 测试后端主机名（deploy-host 下注册 published 端口映射）。
fn prod_backend_host(port: u16) -> String {
    #[cfg(feature = "deploy-host")]
    {
        let host = format!("hdr-timeout-backend-{port}");
        shared_types::published::register_port(&host, port, port);
        host
    }
    #[cfg(not(feature = "deploy-host"))]
    {
        let _ = port;
        "127.0.0.1".to_string()
    }
}

fn prod_backend_unregister(host: &str) {
    #[cfg(feature = "deploy-host")]
    shared_types::published::unregister(host);
    #[cfg(not(feature = "deploy-host"))]
    let _ = host;
}

/// 挂起后端：读到完整请求后按行为分支。
#[derive(Clone, Copy)]
enum HangBehavior {
    /// 永不回响应头（黑洞形态）。
    Silent,
    /// 等 500ms 先回 100-continue，然后每 250ms 滴一个字节的真实响应头行
    /// （永不完成头部）。绝对 deadline 语义的反例锚点：若 1xx 或零碎头
    /// 字节错误地重置计时，deadline 会被推迟/无限延长；`sent_dribble_bytes`
    /// 记录代理终结交换前后端成功滴出的字节数，作为与负载无关的判定信号。
    Dribble1xx,
}

/// 启动挂起后端；`obs.completed` 在每条连接读完**完整请求**（头+body）时递增；
/// `obs.dribble_bytes` 记录 Dribble1xx 形态下实际写出的滴字节总数。
async fn spawn_hang_backend(listener: TcpListener, behavior: HangBehavior) -> Arc<BackendObs> {
    let obs = Arc::new(BackendObs::default());
    let obs_for_task = obs.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            obs_for_task.accepted.fetch_add(1, Ordering::SeqCst);
            let obs = obs_for_task.clone();
            tokio::spawn(async move {
                let header = read_header_bytes(&mut socket).await;
                // 声明了 body 就把 body 读完整（fixture 能解析并接收完整请求）。
                let content_length = header
                    .split(|&b| b == b'\n')
                    .map(|line| String::from_utf8_lossy(line).to_ascii_lowercase())
                    .find_map(|line| {
                        let value = line.strip_prefix("content-length:")?.trim().to_string();
                        value.parse::<usize>().ok()
                    })
                    .unwrap_or(0);
                if content_length > 0 {
                    let mut body = Vec::new();
                    while body.len() < content_length {
                        let mut chunk = [0u8; 4096];
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&chunk[..n]),
                        }
                    }
                }
                obs.completed.fetch_add(1, Ordering::SeqCst);
                match behavior {
                    HangBehavior::Silent => {
                        std::future::pending::<()>().await;
                    }
                    HangBehavior::Dribble1xx => {
                        // 1xx 故意延迟 800ms：绝对 deadline（1s 槽）在 ~1.0s
                        // 终结——后端只来得及滴出 ~2-3 字节（0.8s/1.05s/…）；
                        // 若实现错误地在 1xx 处重置计时，deadline 推到 ~1.8s，
                        // 滴出 ≥4 字节（0.8/1.05/1.3/1.55/1.8）；若每次滴字节
                        // 重置则永不终结（外层 guard 失败）。
                        tokio::time::sleep(Duration::from_millis(800)).await;
                        drop(socket.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await);
                        for byte in b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n" {
                            if socket.write_all(&[*byte]).await.is_err() {
                                break; // 代理已按 deadline 终结并回收上游连接
                            }
                            obs.dribble_bytes.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                        std::future::pending::<()>().await;
                    }
                }
            });
        }
    });
    obs
}

/// 反例（核心，生产默认 60s 预算）：后端完整接收请求后永不回最终响应头。
/// 修复前：代理无限等待（下游只能等外层超时断开）；修复后：按 deadline
/// 返回 504 + 本地错误页（浏览器 HTML/中文文案、no-store、诊断编号），
/// 且等待时长真实跨过 60s（不虚报：响应不得早于预算前出现）。
#[tokio::test]
async fn prod_app_hanging_backend_returns_504_page_within_default_budget() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hanging backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_hang_backend(backend_listener, HangBehavior::Silent).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    manager
        .service
        .set_error_pages(Arc::new(crate::error_page::ErrorPageRenderer::new(None)));
    let backend_host = prod_backend_host(backend_port);
    manager
        .service
        .add_app_backend("hangapp", backend_port, backend_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    stream
        .write_all(
            b"GET /api/v1/userapp/proxy/app/prod/u1/hangapp/ HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Dest: document\r\nAccept: text/html\r\nAccept-Language: zh-CN\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("send request");

    let started = Instant::now();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(75), stream.read_to_end(&mut response))
        .await
        .expect("504 must arrive within the default budget plus slack (no infinite wait)")
        .expect("read response");
    let document = String::from_utf8_lossy(&response).to_string();
    assert!(document.starts_with("HTTP/1.1 504"), "{document}");
    assert!(
        document
            .to_ascii_lowercase()
            .contains("content-type: text/html; charset=utf-8"),
        "{document}"
    );
    assert!(
        document
            .to_ascii_lowercase()
            .contains("cache-control: no-store"),
        "{document}"
    );
    assert!(
        document
            .to_ascii_lowercase()
            .contains("x-rcoder-diagnostic-id: "),
        "{document}"
    );
    // 预算真实生效：不得早于（预算 - 松弛）出现，也不得把等待变成秒回。
    assert!(
        started.elapsed() >= Duration::from_secs(58),
        "response header budget must genuinely wait: elapsed={:?}",
        started.elapsed()
    );
    // 超时档不自动刷新（避免自动重复触发业务）。
    assert!(
        !document
            .to_ascii_lowercase()
            .contains("http-equiv=\"refresh\""),
        "timeout page must not auto refresh: {document}"
    );

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

// ─── 压缩预算（测试槽 1s）的边界矩阵 ───────────────────────────────

/// 应用后端行为（正常业务形态——验证预算不误伤）。
#[derive(Clone, Copy)]
enum AppBehavior {
    /// 立即回 200 ok。
    Ok200,
    /// SSE：立即回 200 + text/event-stream 头，静默 `silence` 后发一段正文
    /// 再关连接——头到即取消 timer，正文静默跨过预算仍能继续传输。
    SseThenChunk { silence: Duration },
    /// Upgrade 请求回 101 后 echo pong（成功升级即止首头阶段）。
    WsEcho,
    /// 只读请求头就立即回 413（上传被上游提前拒绝的真实形态）。
    EarlyReject,
}

/// 无掩码零长度 ping（客户端→服务端）与服务端 pong。
const MASKED_PING: &[u8] = &[0x89, 0x80, 0, 0, 0, 0];
const PONG: [u8; 2] = [0x8a, 0];

async fn spawn_app_backend(listener: TcpListener, behavior: AppBehavior) {
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                match behavior {
                    AppBehavior::Ok200 => {
                        let _header = read_header_bytes(&mut socket).await;
                        drop(socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                            .await);
                    }
                    AppBehavior::SseThenChunk { silence } => {
                        let _header = read_header_bytes(&mut socket).await;
                        drop(socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")
                            .await);
                        tokio::time::sleep(silence).await;
                        drop(socket.write_all(b"data: late\n\n").await);
                    }
                    AppBehavior::WsEcho => {
                        let header = read_header_bytes(&mut socket).await;
                        if String::from_utf8_lossy(&header)
                            .to_ascii_lowercase()
                            .contains("upgrade: websocket")
                        {
                            drop(socket
                                .write_all(
                                    b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
                                )
                                .await);
                            let mut buffer = [0u8; 128];
                            while let Ok(n) = socket.read(&mut buffer).await {
                                if n == 0 {
                                    break;
                                }
                                drop(socket.write_all(&PONG).await);
                            }
                        }
                    }
                    AppBehavior::EarlyReject => {
                        let _header = read_header_bytes(&mut socket).await;
                        drop(socket
                            .write_all(b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 6\r\nConnection: close\r\n\r\ntoobig")
                            .await);
                    }
                }
            });
        }
    });
}

/// 起一套隔离代理（1s 压缩首头预算 + 内置错误页）并注册 prod app 后端；
/// 返回（代理端口, 停止信号, 代理任务）。调用方负责 `prod_backend_unregister`。
async fn start_proxy_with_slot(
    backend_port: u16,
    backend_host: &str,
    app_id: &str,
) -> (u16, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    manager
        .service
        .set_error_pages(Arc::new(crate::error_page::ErrorPageRenderer::new(None)));
    manager
        .service
        .set_userapp_response_header_timeout_for_tests(Duration::from_secs(1));
    manager
        .service()
        .add_app_backend(app_id, backend_port, backend_host.to_string());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;
    (proxy_port, stop, proxy_task)
}

async fn exchange(proxy_port: u16, request: &[u8], guard: Duration) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    stream.write_all(request).await.expect("send request");
    let mut response = Vec::new();
    tokio::time::timeout(guard, stream.read_to_end(&mut response))
        .await
        .expect("response guard deadline")
        .expect("read response");
    String::from_utf8_lossy(&response).to_string()
}

/// 零碎响应头 + 100-continue 不能延长预算：后端 800ms 后回 1xx、再每 250ms
/// 滴一个头字节永不完成。绝对 deadline（1s 槽）内必须 504；且用后端实际
/// 滴出的字节数做**负载无关**判定：绝对 deadline 终结在 ~1.0-1.3s（滴出
/// ≤3 字节）；若 1xx 重置计时则终结在 ~1.8s（≥4 字节）；若每次滴字节重置
/// 则永不终结（外层 guard 失败）。
#[tokio::test]
async fn dribbling_1xx_headers_cannot_extend_the_budget() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind dribble backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    let obs = spawn_hang_backend(backend_listener, HangBehavior::Dribble1xx).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) =
        start_proxy_with_slot(backend_port, &backend_host, "dribble").await;
    let started = Instant::now();
    let response = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/dribble/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(8),
    )
    .await;
    // 后端先行回的 100-continue 会被 Pingora 作为 informational 透传，
    // 最终 504 在其之后到达——断言用 contains 覆盖两种字节序。
    assert!(
        response.contains("HTTP/1.1 504"),
        "504 must terminate the dribbling exchange: {response}"
    );
    // 负载无关锚点：绝对 deadline 终结在 1s 槽附近，后端只来得及滴出极少
    // 字节（首字节 800ms、之后每 250ms 一枚；1xx-重置回归会滴出 ≥4 枚）。
    let dribbled = obs.dribble_bytes.load(Ordering::SeqCst);
    assert!(
        dribbled <= 3,
        "absolute deadline must not be reset by 1xx/dribbled bytes: \
         {dribbled} bytes dribbled before termination, elapsed={:?}",
        started.elapsed()
    );
    assert!(started.elapsed() >= Duration::from_millis(900));

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// SSE 已回 200 头后静默跨过预算（2.5s > 1s 槽）仍能收到迟到正文——
/// 最终头到达即取消 timer，正文静默不受首头预算约束。
#[tokio::test]
async fn sse_header_arrived_then_silence_is_not_killed_by_budget() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sse backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_app_backend(
        backend_listener,
        AppBehavior::SseThenChunk {
            silence: Duration::from_millis(2500),
        },
    )
    .await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) =
        start_proxy_with_slot(backend_port, &backend_host, "sseapp").await;
    let started = Instant::now();
    let response = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/sseapp/events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
        Duration::from_secs(10),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("text/event-stream"), "{response}");
    // 头到即取消 timer：静默跨过预算后迟到的正文仍到达（无 Content-Length
    // 的上游响应被代理按 chunked 重新分帧，正文以 chunk 包裹——不断言尾缀）。
    assert!(response.contains("data: late"), "{response}");
    assert!(
        started.elapsed() >= Duration::from_millis(2400),
        "body silence must genuinely cross the 1s slot: {:?}",
        started.elapsed()
    );

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 成功升级的 WebSocket：101 即结束首头阶段，升级后静默 2.2s（> 1s 槽）再
/// ping 仍能 pong——预算不得延伸到升级后的隧道（下游空闲窗口另有其责）。
#[tokio::test]
async fn websocket_upgrade_ends_header_phase_before_budget() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ws backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_app_backend(backend_listener, AppBehavior::WsEcho).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) =
        start_proxy_with_slot(backend_port, &backend_host, "wsapp").await;

    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    stream
        .write_all(
            b"GET /api/v1/userapp/proxy/app/prod/u1/wsapp/ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .expect("send upgrade request");
    let header = read_header_bytes(&mut stream).await;
    assert!(
        header.starts_with(b"HTTP/1.1 101"),
        "upgrade must succeed before the budget matters: {}",
        String::from_utf8_lossy(&header)
    );

    // 升级后静默跨过 1s 预算槽——连接不得被首头预算关闭。
    tokio::time::sleep(Duration::from_millis(2200)).await;
    stream.write_all(MASKED_PING).await.expect("send ping");
    let mut pong = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut pong))
        .await
        .expect("pong deadline after crossing the budget slot")
        .expect("read pong after crossing the budget slot");
    assert_eq!(pong, PONG);
    drop(stream);

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 上传不计时：1s 槽下慢速上传 3s（每 300ms 一片），后端读完全部 body 才
/// 回 200——200 必须到达（预算自请求 EOS 后起算）。后端提前拒绝（只读头即
/// 413、上传未发完）原样透传，无须强行发完剩余体。
#[tokio::test]
async fn slow_upload_is_not_counted_and_early_rejection_passes_through() {
    // ① 慢上传 + 读完全部再回 200。
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind slow-upload backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    let obs = spawn_hang_then_respond_backend(backend_listener).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) =
        start_proxy_with_slot(backend_port, &backend_host, "uploadapp").await;

    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    stream
        .write_all(
            b"POST /api/v1/userapp/proxy/app/prod/u1/uploadapp/submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3000\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("send upload header");
    let upload_task = tokio::spawn(async move {
        let chunk = [0x61u8; 300];
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if stream.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let mut response = Vec::new();
        use tokio::io::AsyncReadExt as _;
        drop(stream.read_to_end(&mut response).await);
        response
    });
    let response = tokio::time::timeout(Duration::from_secs(15), upload_task)
        .await
        .expect("upload round guard")
        .expect("upload task");
    let text = String::from_utf8_lossy(&response).to_string();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert_eq!(obs.completed.load(Ordering::SeqCst), 1);

    // ② 提前拒绝：客户端只发头 + 首片，后端立即 413——响应到达，无须发完体。
    let reject_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind reject backend");
    let reject_port = reject_listener.local_addr().expect("reject addr").port();
    spawn_app_backend(reject_listener, AppBehavior::EarlyReject).await;
    let reject_host = prod_backend_host(reject_port);
    let (proxy_port2, stop2, proxy_task2) =
        start_proxy_with_slot(reject_port, &reject_host, "rejectapp").await;
    let mut stalled = TcpStream::connect(("127.0.0.1", proxy_port2))
        .await
        .expect("connect proxy");
    stalled
        .write_all(
            b"POST /api/v1/userapp/proxy/app/prod/u1/rejectapp/submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3000\r\nConnection: close\r\n\r\nAAAAA",
        )
        .await
        .expect("send stalled upload head");
    // 请求体未发完、响应先到：连接在代理排空请求侧前不会关——读到完整
    // 413 响应（含正文）即为通过，不等 EOF。
    let response = read_until(&mut stalled, b"toobig", Duration::from_secs(6)).await;
    assert!(
        response.starts_with("HTTP/1.1 413"),
        "early rejection must pass through: {response}"
    );
    assert!(response.ends_with("toobig"), "{response}");

    prod_backend_unregister(&backend_host);
    prod_backend_unregister(&reject_host);
    stop.send(()).expect("stop isolated proxy 1");
    stop2.send(()).expect("stop isolated proxy 2");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline 1")
        .expect("join proxy task 1");
    tokio::time::timeout(Duration::from_secs(10), proxy_task2)
        .await
        .expect("proxy shutdown deadline 2")
        .expect("join proxy task 2");
}

/// 读完整请求（头+body）后回 200 的挂起-再响应后端（上传计时反例用）。
async fn spawn_hang_then_respond_backend(listener: TcpListener) -> Arc<BackendObs> {
    let obs = Arc::new(BackendObs::default());
    let obs_for_task = obs.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let obs = obs_for_task.clone();
            tokio::spawn(async move {
                let header = read_header_bytes(&mut socket).await;
                let content_length = header
                    .split(|&b| b == b'\n')
                    .map(|line| String::from_utf8_lossy(line).to_ascii_lowercase())
                    .find_map(|line| {
                        let value = line.strip_prefix("content-length:")?.trim().to_string();
                        value.parse::<usize>().ok()
                    })
                    .unwrap_or(0);
                let mut body = Vec::new();
                while body.len() < content_length {
                    let mut chunk = [0u8; 4096];
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                    }
                }
                if body.len() >= content_length {
                    obs.completed.fetch_add(1, Ordering::SeqCst);
                    drop(socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                        .await);
                }
            });
        }
    });
    obs
}

/// 在既有连接上读到 `marker` 出现或 EOF 为止（提前拒绝子用例：请求体未
/// 发完，代理排空请求侧前连接不关——不等 EOF）。
async fn read_until(stream: &mut TcpStream, marker: &[u8], guard: Duration) -> String {
    let deadline = tokio::time::Instant::now() + guard;
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(200), stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                response.extend_from_slice(&chunk[..n]);
                if response.windows(marker.len()).any(|w| w == marker) {
                    break;
                }
            }
            Ok(Err(error)) => panic!("read stalled response failed: {error}"),
            Err(_idle) => continue,
        }
    }
    String::from_utf8_lossy(&response).to_string()
}

/// 504 表示与语言矩阵：文档导航 → HTML（zh「应用暂未响应」/ en 英文文案，
/// 均无自动刷新 meta）；fetch（Sec-Fetch-Dest: empty）→ 结构化 JSON，
/// reason 为封闭词 `upstream_response_timeout`；机器形态带 504 状态与
/// 诊断编号，Retry-After 不宣告（避免鼓励立刻重击业务）。
#[tokio::test]
async fn header_timeout_maps_504_for_both_representations_without_autorefresh() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hanging backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_hang_backend(backend_listener, HangBehavior::Silent).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) =
        start_proxy_with_slot(backend_port, &backend_host, "mapapp").await;
    let base = "/api/v1/userapp/proxy/app/prod/u1/mapapp/";

    let zh = exchange(
        proxy_port,
        format!("GET {base} HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Dest: document\r\nSec-Fetch-Mode: navigate\r\nAccept: text/html\r\nAccept-Language: zh-CN\r\nConnection: close\r\n\r\n").as_bytes(),
        Duration::from_secs(8),
    )
    .await;
    assert!(zh.starts_with("HTTP/1.1 504"), "{zh}");
    assert!(
        zh.to_ascii_lowercase()
            .contains("content-type: text/html; charset=utf-8"),
        "{zh}"
    );
    assert!(zh.contains("应用暂未响应"), "{zh}");
    assert!(
        !zh.to_ascii_lowercase().contains("http-equiv=\"refresh\""),
        "timeout tier must not auto refresh: {zh}"
    );
    assert!(
        !zh.to_ascii_lowercase().contains("retry-after"),
        "header timeout tier must not advertise an auto-retry cadence: {zh}"
    );

    let en = exchange(
        proxy_port,
        format!("GET {base} HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Dest: document\r\nSec-Fetch-Mode: navigate\r\nAccept: text/html\r\nAccept-Language: en-US\r\nConnection: close\r\n\r\n").as_bytes(),
        Duration::from_secs(8),
    )
    .await;
    assert!(en.starts_with("HTTP/1.1 504"), "{en}");
    assert!(en.contains("App not responding yet"), "{en}");

    let fetch = exchange(
        proxy_port,
        format!("GET {base}assets/app.js HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Dest: empty\r\nSec-Fetch-Mode: cors\r\nAccept: */*\r\nConnection: close\r\n\r\n").as_bytes(),
        Duration::from_secs(8),
    )
    .await;
    assert!(fetch.starts_with("HTTP/1.1 504"), "{fetch}");
    assert!(
        fetch
            .to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{fetch}"
    );
    assert!(
        fetch.contains("\"reason\":\"upstream_response_timeout\""),
        "{fetch}"
    );
    assert!(fetch.contains("\"status\":504"), "{fetch}");

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// POST 超时不自动重放：挂死后端只应看到**一条**完整请求（预算耗尽是终局，
/// 不新增后台重试）；结果未知不写成业务失败结果——机器 reason 是阶段超时
/// 而非应用返回的错误。
#[tokio::test]
async fn post_timeout_does_not_replay_the_upstream_request() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hanging backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    let obs = spawn_hang_backend(backend_listener, HangBehavior::Silent).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) =
        start_proxy_with_slot(backend_port, &backend_host, "postapp").await;
    let response = exchange(
        proxy_port,
        b"POST /api/v1/userapp/proxy/app/prod/u1/postapp/submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
        Duration::from_secs(8),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 504"), "{response}");
    assert_eq!(
        obs.completed.load(Ordering::SeqCst),
        1,
        "timeout must not replay the POST"
    );

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 超时后的连接回收不污染后续请求：同一代理上 504 一次（挂起 app），随后
/// 另一 app 的普通 200 请求完整往返（旧 timer 不得误杀下一次交换）。
#[tokio::test]
async fn request_after_timeout_is_not_poisoned_by_the_old_timer() {
    let hang_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hang backend");
    let hang_port = hang_listener.local_addr().expect("hang addr").port();
    spawn_hang_backend(hang_listener, HangBehavior::Silent).await;
    let ok_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ok backend");
    let ok_port = ok_listener.local_addr().expect("ok addr").port();
    spawn_app_backend(ok_listener, AppBehavior::Ok200).await;

    let hang_host = prod_backend_host(hang_port);
    let ok_host = prod_backend_host(ok_port);
    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    manager
        .service
        .set_error_pages(Arc::new(crate::error_page::ErrorPageRenderer::new(None)));
    manager
        .service
        .set_userapp_response_header_timeout_for_tests(Duration::from_secs(1));
    manager
        .service()
        .add_app_backend("hang2", hang_port, hang_host.clone());
    manager
        .service()
        .add_app_backend("fine", ok_port, ok_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let first = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/hang2/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(8),
    )
    .await;
    assert!(first.starts_with("HTTP/1.1 504"), "{first}");

    let second = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/fine/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(6),
    )
    .await;
    assert!(second.starts_with("HTTP/1.1 200"), "{second}");
    assert!(second.ends_with("ok"), "{second}");

    prod_backend_unregister(&hang_host);
    prod_backend_unregister(&ok_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 重试预算共享后端：连接 #1 服务一次 keep-alive 预热 GET 后被 PUT 复用——
/// 完整读取 PUT、700ms 后以 RST（SO_LINGER=0）断开，触发 Pingora 既有
/// ReusedOnly 重试（幂等 PUT + 请求体缓冲重放）；连接 #2 完整接收重放的
/// PUT 后黑洞。`completed` 只计连接 #2（重试到达的证明）。
async fn spawn_retry_budget_backend(listener: TcpListener) -> Arc<BackendObs> {
    let obs = Arc::new(BackendObs::default());
    let obs_for_task = obs.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let ordinal = obs_for_task.accepted.fetch_add(1, Ordering::SeqCst) + 1;
            let obs = obs_for_task.clone();
            tokio::spawn(async move {
                if ordinal == 1 {
                    // 预热 GET：keep-alive 响应让代理把该连接放回上游池
                    let _warmup = read_header_bytes(&mut socket).await;
                    drop(
                        socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .await,
                    );
                    // 同一连接上的复用 PUT：读完整请求（刻意留 1 字节不读——
                    // 关闭时接收缓冲非空会让内核发 RST 而非 FIN），700ms 后断开
                    let header = read_header_bytes(&mut socket).await;
                    let content_length = header
                        .split(|&b| b == b'\n')
                        .map(|line| String::from_utf8_lossy(line).to_ascii_lowercase())
                        .find_map(|line| {
                            let value = line.strip_prefix("content-length:")?.trim().to_string();
                            value.parse::<usize>().ok()
                        })
                        .unwrap_or(0);
                    let mut body = Vec::new();
                    while body.len() + 1 < content_length {
                        let want = (content_length - 1 - body.len()).min(4096);
                        let mut chunk = vec![0u8; want];
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&chunk[..n]),
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    drop(socket);
                } else {
                    // 重试连接：完整接收重放的 PUT 后黑洞
                    let header = read_header_bytes(&mut socket).await;
                    let content_length = header
                        .split(|&b| b == b'\n')
                        .map(|line| String::from_utf8_lossy(line).to_ascii_lowercase())
                        .find_map(|line| {
                            let value = line.strip_prefix("content-length:")?.trim().to_string();
                            value.parse::<usize>().ok()
                        })
                        .unwrap_or(0);
                    let mut body = Vec::new();
                    while body.len() < content_length {
                        let mut chunk = [0u8; 4096];
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&chunk[..n]),
                        }
                    }
                    obs.completed.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                }
            });
        }
    });
    obs
}

/// 重试准入墙钟的真实回归（独立复审边界项）：复用连接在请求写出后
/// ~700ms 被 RST → 幂等 PUT 走 Pingora 既有安全重试 → 重放连接黑洞。
/// 准入墙钟语义（首次选 peer 起扣减、重试分配剩余）下 504 在 ~1s（1s
/// 槽）附近；若每次重连重新获得完整预算，则要在重放 EOS（~0.75s）之后
/// 再等满 1s（~1.75s）——1400ms 上限区分两者。全程经真实代理链路（含
/// 连接池复用、RST 错误分类与请求体缓冲重放），非 helper sleep 等价。
#[tokio::test]
async fn retry_after_reused_connection_reset_shares_the_header_budget() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind retry backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    let obs = spawn_retry_budget_backend(backend_listener).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) =
        start_proxy_with_slot(backend_port, &backend_host, "retryapp").await;

    // 预热：一次 GET 让上游连接进入代理连接池（keep-alive）
    let warmup = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/retryapp/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        Duration::from_secs(6),
    )
    .await;
    assert!(warmup.starts_with("HTTP/1.1 200"), "{warmup}");

    // 幂等 PUT + 一次性完整 body（代理缓冲后写往复用连接）
    let body = vec![b'p'; 1024];
    let mut request = format!(
        "PUT /api/v1/userapp/proxy/app/prod/u1/retryapp/submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);

    let started = Instant::now();
    let response = exchange(proxy_port, &request, Duration::from_secs(6)).await;
    let elapsed = started.elapsed();
    assert!(
        response.starts_with("HTTP/1.1 504"),
        "shared budget must end in the timeout tier: {response}"
    );
    assert!(
        response.contains("\"reason\":\"upstream_response_timeout\""),
        "{response}"
    );
    assert!(
        elapsed >= Duration::from_millis(950) && elapsed <= Duration::from_millis(1400),
        "retry must share the remaining wall budget from first peer selection \
         (fresh full budget after replay would land ~1.75s): {elapsed:?}"
    );
    assert_eq!(
        obs.accepted.load(Ordering::SeqCst),
        2,
        "exactly one warmup+PUT connection and one retry connection"
    );
    assert_eq!(
        obs.completed.load(Ordering::SeqCst),
        1,
        "the replayed PUT must reach the retry connection in full"
    );

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}
