//! UserApp app 路由官方上游读空闲窗口回归（specs/userapp-official-
//! pingora-timeout-20261031）。
//!
//! 生产症状（specs/userapp-response-header-timeout-20261030 同源）：请求
//! 已发往上游（pingap 9080），始终收不到响应——外层 OpenResty 约 10 分钟
//! 才断开，浏览器长期白屏。本批**回归官方 Pingora**（撤除
//! vendor/pingora-core、vendor/pingora-proxy 的 `response_header_timeout`
//! 补丁及自定义重试准入墙钟），改为官方 `PeerOptions::read_timeout`
//! （[`PingoraProxyService::DEFAULT_USERAPP_UPSTREAM_READ_TIMEOUT`]，候选
//! 值 300s）：
//!
//! - **逐次读取计时**：每次成功读到上游字节（响应头或正文）即重新起算；
//!   不是连接总寿命，也不是整次请求的绝对期限。持续有数据的 SSE/正文
//!   可运行远超窗口（反例见 `sse_with_progress_runs_past_the_read_window`，
//!   官方无补丁实验 250ms 窗口下 SSE/WS 各持续 71.9s 完整收到 1000 条
//!   事件/帧——specs/pingora-official-research-20261031）；
//! - **静默超窗即断**：已开始（200）的响应按官方超时结束流，状态保持
//!   200、**不追加 HTML**（`sse_silence_after_first_event_...`）；未开始
//!   最终响应的交换映射 504 + 既有本地错误页（HTML/JSON、三语、no-store、
//!   诊断编号、不自动刷新、不宣告 Retry-After）；
//! - **WebSocket 升级请求豁免**（HTTP/1.1 + `Upgrade: websocket`，任意
//!   Upgrade token 不豁免——`non_websocket_upgrade_token_...`），保留既有
//!   长连接 peer 行为与升级后下游空闲窗口；
//! - **上传语义（官方 relay 实测）**：客户端**持续发送**的上传不受窗口
//!   误伤——`proxy_handle_upstream` 的 `tokio::select!` 在下游 body 每片
//!   写入后重建上游读 future，读窗口计时随之重置；客户端**中途停住**且
//!   上游零响应字节的交换如实超时 504
//!   （`upload_active_survives_the_window_and_stalled_hits_the_window`）；
//! - **重试无共享墙钟**：官方安全重试（幂等方法、ReusedOnly、请求体
//!   缓冲重放）保留，每次尝试获得同一完整窗口
//!   （`retry_after_reused_connection_reset_...`）；POST 超时不自动重放。
//!
//! 全部用例经真实 `PingoraServerManager`（`ProxyServiceWrapper` →
//! `PortProxy`）与真实 dev/prod app 代理路由。压缩用例用测试槽
//! （`set_userapp_upstream_read_timeout_for_tests`）受控验证边界分支；
//! 生产默认 300s 的真实计时反例
//! `real_default_window_blackhole_times_out_at_300s` 为显式 `#[ignore]`
//! 用例——常规套件跳过且**计为 ignored（非 PASS）**，交付验收轮以
//! `--run-ignored only` 显式选中执行并记录各阶段耗时，不以此处宣称
//! 已验证。

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

/// 后端观测：`completed` 计读完整个请求（头+body）的连接数，
/// `accepted` 计收到的连接总数（重试次数断言用）。
#[derive(Default)]
struct BackendObs {
    completed: AtomicUsize,
    accepted: AtomicUsize,
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

/// 预留并释放一个本机代理端口（内核临时段以下显式端口，避免与并发
/// `bind(:0)` 监听者互抢——满载全量跑实测会撞入别人的代理）。
fn reserve_port() -> u16 {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64
        | 1;
    for _ in 0..16 {
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
        let host = format!("read-timeout-backend-{port}");
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

/// 启动黑洞后端：读到完整请求（头+body）后永不回任何字节。
async fn spawn_blackhole_backend(listener: TcpListener) -> Arc<BackendObs> {
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
            });
        }
    });
    obs
}

/// 读完整请求（头+body）后回 200 的后端（合理窗口内上传反例用）。
async fn spawn_upload_backend(listener: TcpListener) -> Arc<BackendObs> {
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

/// 应用后端行为（验证窗口不误伤/例外矩阵）。
#[derive(Clone, Copy)]
enum AppBehavior {
    /// 立即回 200 ok。
    Ok200,
    /// SSE：立即回 200 + text/event-stream 头，之后每 `gap` 发一条事件，
    /// 共 `events` 条再关连接——持续读取进展下总时长可跨过多个窗口。
    SseProgress { gap: Duration, events: usize },
    /// SSE：回 200 + 头部 + 首条事件后永久静默（静默超窗反例）。
    SseFirstThenSilent,
    /// Upgrade: websocket 请求回 101 后 echo pong。
    WsEcho,
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
                    AppBehavior::SseProgress { gap, events } => {
                        let _header = read_header_bytes(&mut socket).await;
                        drop(socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")
                            .await);
                        for index in 0..events {
                            tokio::time::sleep(gap).await;
                            if socket
                                .write_all(format!("data: {index}\n\n").as_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    AppBehavior::SseFirstThenSilent => {
                        let _header = read_header_bytes(&mut socket).await;
                        drop(socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\ndata: first\n\n")
                            .await);
                        std::future::pending::<()>().await;
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
                }
            });
        }
    });
}

/// 起一套隔离代理（压缩读窗口 + 内置错误页）并注册 prod app 后端；
/// 返回（代理端口, 停止信号, 代理任务）。调用方负责 `prod_backend_unregister`。
async fn start_proxy_with_slot(
    backend_port: u16,
    backend_host: &str,
    app_id: &str,
    slot: Duration,
) -> (u16, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    manager
        .service
        .set_error_pages(Arc::new(crate::error_page::ErrorPageRenderer::new(None)));
    manager
        .service
        .set_userapp_upstream_read_timeout_for_tests(slot);
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

async fn stop_proxy(stop: oneshot::Sender<()>, proxy_task: tokio::task::JoinHandle<()>) {
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 压缩窗口（1s 槽）核心反例：后端完整接收请求后永不回任何字节（黑洞）。
/// 官方 `read_timeout` 逐次读取计时——无任何上游字节可读，1s 窗口耗尽后
/// `ReadTimedout`（Upstream）映射 504 + 本地错误页（中文文案、no-store、
/// 诊断编号），且等待真实跨过窗口（不虚报：响应不得早于窗口出现）。
#[tokio::test]
async fn prod_app_blackhole_returns_504_page_within_the_window() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blackhole backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_blackhole_backend(backend_listener).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) = start_proxy_with_slot(
        backend_port,
        &backend_host,
        "holeapp",
        Duration::from_secs(1),
    )
    .await;

    let started = Instant::now();
    let response = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/holeapp/ HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Dest: document\r\nAccept: text/html\r\nAccept-Language: zh-CN\r\nConnection: close\r\n\r\n",
        Duration::from_secs(8),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 504"), "{response}");
    assert!(
        response
            .to_ascii_lowercase()
            .contains("content-type: text/html; charset=utf-8"),
        "{response}"
    );
    assert!(
        response
            .to_ascii_lowercase()
            .contains("cache-control: no-store"),
        "{response}"
    );
    assert!(
        response
            .to_ascii_lowercase()
            .contains("x-rcoder-diagnostic-id: "),
        "{response}"
    );
    assert!(response.contains("应用暂未响应"), "{response}");
    assert!(
        started.elapsed() >= Duration::from_millis(950),
        "the read window must genuinely elapse: {:?}",
        started.elapsed()
    );
    // 超时档不自动刷新（避免自动重复触发业务，特别是可能仍在处理中的写请求）。
    assert!(
        !response
            .to_ascii_lowercase()
            .contains("http-equiv=\"refresh\""),
        "timeout page must not auto refresh: {response}"
    );

    prod_backend_unregister(&backend_host);
    stop_proxy(stop, proxy_task).await;
}

/// 生产默认 300s 候选值的真实计时反例：黑洞请求按默认窗口真实等待约
/// 300s 后 504。常规套件不跑 5 分钟长用例（显式 `#[ignore]`——缺环境
/// 变量式早退会被计成 PASS，不用）；交付验收轮显式选中执行并记录耗时
/// （请求写出 → 504 完成）：
/// `cargo nextest run -p rcoder-proxy --run-ignored only -E 'test(real_default_window)'`
#[ignore = "5-minute real-window acceptance case; run explicitly with --run-ignored only"]
#[tokio::test]
async fn real_default_window_blackhole_times_out_at_300s() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blackhole backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_blackhole_backend(backend_listener).await;

    let backend_host = prod_backend_host(backend_port);
    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    manager
        .service
        .set_error_pages(Arc::new(crate::error_page::ErrorPageRenderer::new(None)));
    manager
        .service()
        .add_app_backend("realwin", backend_port, backend_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let started = Instant::now();
    let response = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/realwin/ HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Dest: document\r\nAccept: text/html\r\nAccept-Language: zh-CN\r\nConnection: close\r\n\r\n",
        Duration::from_secs(330),
    )
    .await;
    let elapsed = started.elapsed();
    assert!(response.starts_with("HTTP/1.1 504"), "{response}");
    assert!(
        elapsed >= Duration::from_secs(295),
        "the default 300s window must genuinely elapse: {elapsed:?}"
    );
    eprintln!("real 300s window blackhole: 504 complete at {elapsed:?}");

    prod_backend_unregister(&backend_host);
    stop_proxy(stop, proxy_task).await;
}

/// dev 入口同窗口策略：dev app 代理（IPv6 回环 ::1:APP_ENTRY_PORT——避开
/// 127.0.0.1 固定端口被本机真实开发流量争用）上黑洞同样 504。
#[tokio::test]
async fn dev_app_blackhole_returns_504_through_the_dev_entry() {
    // [::1]:APP_ENTRY_PORT 固定端口——冲突时重试等待其他 dev 用例释放
    //（见 pingora_server::dev_entry_port）。
    let backend = dev_entry_port::bind_backend().await;
    spawn_blackhole_backend(backend).await;

    #[cfg(feature = "deploy-host")]
    shared_types::published::register_direct(
        "::1",
        "::1".parse::<std::net::IpAddr>().expect("IPv6 loopback"),
    );
    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port));
    manager.service().set_dev_ensure(Arc::new(FixedDevEnsure));
    manager
        .service
        .set_error_pages(Arc::new(crate::error_page::ErrorPageRenderer::new(None)));
    manager
        .service
        .set_userapp_upstream_read_timeout_for_tests(Duration::from_secs(1));
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let response = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/dev/u1/devhole/ HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Dest: document\r\nAccept: text/html\r\nAccept-Language: zh-CN\r\nConnection: close\r\n\r\n",
        Duration::from_secs(8),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 504"), "{response}");
    assert!(response.contains("应用暂未响应"), "{response}");
    assert!(
        !response
            .to_ascii_lowercase()
            .contains("http-equiv=\"refresh\""),
        "timeout page must not auto refresh: {response}"
    );

    stop_proxy(stop, proxy_task).await;
}

/// dev 定位桩：所有 app 指向本测试的隔离后端（IPv6 回环 ::1）。只定位、
/// 不创建——超时回归不得触碰真实计算资源。
struct FixedDevEnsure;

#[async_trait::async_trait]
impl shared_types::UserappDevEnsure for FixedDevEnsure {
    async fn locate_dev_builder(
        &self,
        _app_id: &str,
    ) -> Result<Option<shared_types::DevBuilderInstance>, shared_types::DevEnsureError> {
        Ok(Some(shared_types::DevBuilderInstance {
            address: "::1".into(),
            container_id: "read-timeout-fixture".into(),
        }))
    }

    async fn ensure_dev_container(
        &self,
        _app_id: &str,
    ) -> Result<shared_types::ContainerBasicInfo, shared_types::DevEnsureError> {
        panic!("read timeout regression must not create real compute containers");
    }
}

/// 504 表示与语言矩阵：文档导航 → HTML（zh「应用暂未响应」/ en 英文文案，
/// 均无自动刷新 meta）；fetch（Sec-Fetch-Dest: empty）→ 结构化 JSON，
/// reason 为封闭词 `upstream_response_timeout`；机器形态带 504 状态与
/// 诊断编号，Retry-After 不宣告（避免鼓励立刻重击业务）。
#[tokio::test]
async fn read_timeout_maps_504_for_both_representations_without_autorefresh() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blackhole backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_blackhole_backend(backend_listener).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) = start_proxy_with_slot(
        backend_port,
        &backend_host,
        "mapapp",
        Duration::from_secs(1),
    )
    .await;
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
        "timeout tier must not advertise an auto-retry cadence: {zh}"
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
    stop_proxy(stop, proxy_task).await;
}

/// POST 超时不自动重放：黑洞后端只应看到**一条**完整请求（窗口耗尽是
/// 终局，不新增后台重试）；结果未知不写成业务失败结果——机器 reason 是
/// 阶段超时而非应用返回的错误。
#[tokio::test]
async fn post_timeout_does_not_replay_the_upstream_request() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blackhole backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    let obs = spawn_blackhole_backend(backend_listener).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) = start_proxy_with_slot(
        backend_port,
        &backend_host,
        "postapp",
        Duration::from_secs(1),
    )
    .await;
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
    assert_eq!(
        obs.accepted.load(Ordering::SeqCst),
        1,
        "blackhole POST must not spawn extra upstream attempts"
    );

    prod_backend_unregister(&backend_host);
    stop_proxy(stop, proxy_task).await;
}

/// 超时后的连接回收不污染后续请求：同一代理上 504 一次（黑洞 app），随后
/// 另一 app 的普通 200 请求完整往返（旧超时不得误杀下一次交换）。
#[tokio::test]
async fn request_after_timeout_is_not_poisoned_by_the_old_timer() {
    let hang_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blackhole backend");
    let hang_port = hang_listener.local_addr().expect("hang addr").port();
    spawn_blackhole_backend(hang_listener).await;
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
        .set_userapp_upstream_read_timeout_for_tests(Duration::from_secs(1));
    manager
        .service()
        .add_app_backend("hole2", hang_port, hang_host.clone());
    manager
        .service()
        .add_app_backend("fine", ok_port, ok_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let first = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/hole2/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
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
    stop_proxy(stop, proxy_task).await;
}

/// 持续读取进展可跨过多个窗口（官方语义反例，压缩形态）：1s 窗口下 SSE
/// 每 300ms 一条事件、共 12 条（总时长 ≈3.6s ≈ 3.6 个窗口），全部到达且
/// 状态 200——`read_timeout` 是逐次读取空闲上限，不是连接/请求总时长。
/// （官方无补丁实验同机制：250ms 窗口下 1000 条事件持续 71.9s 完整转发。）
#[tokio::test]
async fn sse_with_progress_runs_past_the_read_window() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sse backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_app_backend(
        backend_listener,
        AppBehavior::SseProgress {
            gap: Duration::from_millis(300),
            events: 12,
        },
    )
    .await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) = start_proxy_with_slot(
        backend_port,
        &backend_host,
        "sseapp",
        Duration::from_secs(1),
    )
    .await;
    let started = Instant::now();
    let response = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/sseapp/events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
        Duration::from_secs(15),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("text/event-stream"), "{response}");
    for index in 0..12 {
        assert!(
            response.contains(&format!("data: {index}\n")),
            "event {index} missing (read window is not a total-duration cap): {response}"
        );
    }
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "total stream time must genuinely cross multiple 1s windows: {:?}",
        started.elapsed()
    );

    prod_backend_unregister(&backend_host);
    stop_proxy(stop, proxy_task).await;
}

/// 已开始响应的静默断流：200 + 首条事件后永久静默，1s 窗口耗尽按官方
/// 超时结束——状态保持 200、已收字节完整送达、**不追加 HTML/504**（响应
/// 已开始，错误页管线不得在流中途注入）。
#[tokio::test]
async fn sse_silence_after_first_event_closes_at_the_window_without_html() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sse backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_app_backend(backend_listener, AppBehavior::SseFirstThenSilent).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) = start_proxy_with_slot(
        backend_port,
        &backend_host,
        "silentapp",
        Duration::from_secs(1),
    )
    .await;
    let started = Instant::now();
    let response = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/silentapp/events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
        Duration::from_secs(8),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("data: first"), "{response}");
    // 已开始的响应不得中途切换错误页：无 HTML、无第二状态行。
    assert!(
        !response.to_ascii_lowercase().contains("<!doctype"),
        "no HTML may be appended to a started stream: {response}"
    );
    assert!(
        response.matches("HTTP/1.1").count() == 1,
        "no second status line may appear mid-stream: {response}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(950)
            && started.elapsed() <= Duration::from_secs(4),
        "the stream must end at the read window, not instantly and not never: {:?}",
        started.elapsed()
    );

    prod_backend_unregister(&backend_host);
    stop_proxy(stop, proxy_task).await;
}

/// WebSocket 升级请求豁免读窗口：101 后静默跨过 1s 窗口再 ping 仍能
/// pong——升级隧道不受上游读窗口约束（下游空闲窗口另有
/// `DEFAULT_UPGRADED_DOWNSTREAM_READ_TIMEOUT` 负责）。
#[tokio::test]
async fn websocket_upgrade_is_exempt_from_the_read_window() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ws backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_app_backend(backend_listener, AppBehavior::WsEcho).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) =
        start_proxy_with_slot(backend_port, &backend_host, "wsapp", Duration::from_secs(1)).await;

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
        "upgrade must succeed before the window matters: {}",
        String::from_utf8_lossy(&header)
    );

    // 升级后上游静默跨过 1s 窗口——连接不得被读窗口关闭。
    tokio::time::sleep(Duration::from_millis(2200)).await;
    stream.write_all(MASKED_PING).await.expect("send ping");
    let mut pong = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut pong))
        .await
        .expect("pong deadline after crossing the read window")
        .expect("read pong after crossing the read window");
    assert_eq!(pong, PONG);
    drop(stream);

    prod_backend_unregister(&backend_host);
    stop_proxy(stop, proxy_task).await;
}

/// 任意 Upgrade token 不豁免：`Upgrade: h2c`（非 websocket 协议字段）的
/// app 请求照常套读窗口——黑洞 1s 内 504（升级判定必须核对 websocket
/// 协议字段，不能凭任意 Upgrade 头放行）。
#[tokio::test]
async fn non_websocket_upgrade_token_still_gets_the_read_window() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blackhole backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_blackhole_backend(backend_listener).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) = start_proxy_with_slot(
        backend_port,
        &backend_host,
        "h2capp",
        Duration::from_secs(1),
    )
    .await;
    let response = exchange(
        proxy_port,
        b"GET /api/v1/userapp/proxy/app/prod/u1/h2capp/ HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: h2c\r\nHTTP2-Settings: dummy\r\nConnection: close\r\n\r\n",
        Duration::from_secs(8),
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 504"),
        "non-websocket Upgrade requests must keep the read window: {response}"
    );

    prod_backend_unregister(&backend_host);
    stop_proxy(stop, proxy_task).await;
}

/// 上传三档（官方语义实测）：
/// ① 快于窗口的上传（1.6s < 3s 窗口）正常完成——后端读完全部 body 后
///    回 200，200 必须到达；
/// ② **持续**慢于窗口的上传（3s > 1s 窗口）同样完成 200：官方 relay
///    （`proxy_handle_upstream` 的 `tokio::select!`）在下游 body 每片写入
///    后重建上游读 future——客户端持续发送会不断重置读窗口计时，活跃
///    上传不受窗口误伤；
/// ③ **中途停住**的上传 + 上游全程零响应字节——读窗口如实超时 504
///    （官方无补丁实验"上传 EOS 前也可触发读超时"同形态；不再保留
///    "任何时长上传都不计时"的旧声明，也不虚报持续上传会被误杀）。
#[tokio::test]
async fn upload_active_survives_the_window_and_stalled_hits_the_window() {
    // ① 快于窗口：4 片 × 400ms = 1.6s < 3s 窗口。
    let fast_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fast-upload backend");
    let fast_port = fast_listener.local_addr().expect("fast addr").port();
    let fast_obs = spawn_upload_backend(fast_listener).await;
    let fast_host = prod_backend_host(fast_port);
    let (proxy_port, stop, proxy_task) =
        start_proxy_with_slot(fast_port, &fast_host, "fastup", Duration::from_secs(3)).await;

    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    stream
        .write_all(
            b"POST /api/v1/userapp/proxy/app/prod/u1/fastup/submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1200\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("send upload header");
    let upload_task = tokio::spawn(async move {
        let chunk = [0x61u8; 300];
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(400)).await;
            if stream.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let mut response = Vec::new();
        drop(stream.read_to_end(&mut response).await);
        response
    });
    let response = tokio::time::timeout(Duration::from_secs(15), upload_task)
        .await
        .expect("upload round guard")
        .expect("upload task");
    let text = String::from_utf8_lossy(&response).to_string();
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "upload faster than the window must complete: {text}"
    );
    assert_eq!(fast_obs.completed.load(Ordering::SeqCst), 1);

    prod_backend_unregister(&fast_host);
    stop_proxy(stop, proxy_task).await;

    // ② 持续慢于窗口：10 片 × 300ms = 3s > 1s 窗口，后端等完整 body 才
    //    响应（上游全程零字节）——客户端持续发送重置读计时，上传完整
    //    完成后 200 到达。
    let steady_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind steady-upload backend");
    let steady_port = steady_listener.local_addr().expect("steady addr").port();
    let steady_obs = spawn_upload_backend(steady_listener).await;
    let steady_host = prod_backend_host(steady_port);
    let (proxy_port2, stop2, proxy_task2) = start_proxy_with_slot(
        steady_port,
        &steady_host,
        "steadyup",
        Duration::from_secs(1),
    )
    .await;

    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port2))
        .await
        .expect("connect proxy");
    stream
        .write_all(
            b"POST /api/v1/userapp/proxy/app/prod/u1/steadyup/submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3000\r\nConnection: close\r\n\r\n",
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
        drop(stream.read_to_end(&mut response).await);
        response
    });
    let response = tokio::time::timeout(Duration::from_secs(15), upload_task)
        .await
        .expect("steady upload round guard")
        .expect("steady upload task");
    let text = String::from_utf8_lossy(&response).to_string();
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "a continuously sending upload must not be killed by the read window \
         (relay activity re-arms the per-read timer): {text}"
    );
    assert_eq!(steady_obs.completed.load(Ordering::SeqCst), 1);

    prod_backend_unregister(&steady_host);
    stop_proxy(stop2, proxy_task2).await;

    // ③ 中途停住：客户端只发头 + 首片后停止，后端等完整 body 才响应
    //    （双方均无进展）——读窗口耗尽 504。
    let stalled_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalled-upload backend");
    let stalled_port = stalled_listener.local_addr().expect("stalled addr").port();
    let stalled_obs = spawn_upload_backend(stalled_listener).await;
    let stalled_host = prod_backend_host(stalled_port);
    let (proxy_port3, stop3, proxy_task3) = start_proxy_with_slot(
        stalled_port,
        &stalled_host,
        "stallup",
        Duration::from_secs(1),
    )
    .await;

    let mut stalled = TcpStream::connect(("127.0.0.1", proxy_port3))
        .await
        .expect("connect proxy");
    stalled
        .write_all(
            b"POST /api/v1/userapp/proxy/app/prod/u1/stallup/submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3000\r\nConnection: close\r\n\r\nAAAAA",
        )
        .await
        .expect("send stalled upload head");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), stalled.read_to_end(&mut response))
        .await
        .expect("stalled upload response deadline")
        .expect("read stalled upload response");
    let text = String::from_utf8_lossy(&response).to_string();
    assert!(
        text.starts_with("HTTP/1.1 504"),
        "a stalled upload with a silent upstream must hit the official read window: {text}"
    );
    assert!(
        text.contains("\"reason\":\"upstream_response_timeout\"") || text.contains("应用暂未响应"),
        "{text}"
    );
    assert_eq!(
        stalled_obs.completed.load(Ordering::SeqCst),
        0,
        "the backend must never see the full request (window fires mid-upload)"
    );

    prod_backend_unregister(&stalled_host);
    stop_proxy(stop3, proxy_task3).await;
}

/// 重试预算后端：连接 #1 服务一次 keep-alive 预热 GET 后被 PUT 复用——
/// 完整读取 PUT、700ms 后以 RST（SO_LINGER=0）断开，触发 Pingora 既有
/// ReusedOnly 安全重试（幂等 PUT + 请求体缓冲重放）；连接 #2 完整接收
/// 重放的 PUT 后黑洞。`completed` 只计连接 #2（重试到达的证明）。
async fn spawn_retry_backend(listener: TcpListener) -> Arc<BackendObs> {
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

/// 官方安全重试保留 + 无共享墙钟：复用连接在请求写出后 ~700ms 被 RST →
/// 幂等 PUT 走 Pingora 既有安全重试（缓冲重放到达连接 #2）→ 重试连接
/// 黑洞。每次尝试获得**同一完整窗口**：重试自获准起重新计时，504 在
/// ~1.7s（0.7s RST + 1s 重试窗口）附近；无墙钟语义下不存在"共享剩余"
/// 的更早截断。全程经真实代理链路（含连接池复用、RST 错误分类与请求体
/// 缓冲重放），非 helper sleep 等价。
#[tokio::test]
async fn retry_after_reused_connection_reset_gets_a_fresh_window() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind retry backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    let obs = spawn_retry_backend(backend_listener).await;

    let backend_host = prod_backend_host(backend_port);
    let (proxy_port, stop, proxy_task) = start_proxy_with_slot(
        backend_port,
        &backend_host,
        "retryapp",
        Duration::from_secs(1),
    )
    .await;

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
    let response = exchange(proxy_port, &request, Duration::from_secs(8)).await;
    let elapsed = started.elapsed();
    assert!(
        response.starts_with("HTTP/1.1 504"),
        "the retried attempt must end in the timeout tier: {response}"
    );
    assert!(
        response.contains("\"reason\":\"upstream_response_timeout\""),
        "{response}"
    );
    assert!(
        elapsed >= Duration::from_millis(1600),
        "the retry attempt must get its own full window after the ~700ms RST \
         (a shared wall clock would cut it earlier): {elapsed:?}"
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
    stop_proxy(stop, proxy_task).await;
}

/// 非 UserApp 路由不受窗口策略影响：`/proxy/{port}`（端口转发族，
/// `streaming_peer_options` 保持 `read_timeout = None`）上黑洞后端在
/// 2.5s 观察窗内不得出现任何响应字节（含 504）——全局不误伤 LLM/VNC/
/// ttyd/dbx/端口转发等长连接路由。
#[tokio::test]
async fn non_userapp_port_route_keeps_its_unlimited_read_policy() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind blackhole backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_blackhole_backend(backend_listener).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    // 与 app 路由同代理同窗口（1s）注册——若策略误套全局，黑洞会在 1s
    // 出现 504；正确行为是端口转发保持无上游读超时（本用例的观察窗内
    // 挂起，客户端侧超时退出）。deploy-host 下后端地址经 published 注册
    //（同 prod 后端模式）。
    manager
        .service
        .set_userapp_upstream_read_timeout_for_tests(Duration::from_secs(1));
    let forward_host = prod_backend_host(backend_port);
    manager
        .service()
        .add_backend(backend_port, forward_host.clone())
        .await;
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    stream
        .write_all(
            format!("GET /proxy/{backend_port}/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .expect("send request");
    let mut response = Vec::new();
    let outcome = tokio::time::timeout(Duration::from_millis(2500), async {
        loop {
            let mut chunk = [0u8; 512];
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
    })
    .await;
    assert!(
        outcome.is_err() && response.is_empty(),
        "the port-forward route must stay unlimited (no bytes, no early 504): {}",
        String::from_utf8_lossy(&response)
    );

    prod_backend_unregister(&forward_host);
    stop_proxy(stop, proxy_task).await;
}
