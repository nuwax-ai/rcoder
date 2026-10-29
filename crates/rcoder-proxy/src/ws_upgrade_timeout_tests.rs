//! 批次一回归：成功升级的 UserApp WebSocket 不被下游默认 60s 读超时截断。
//!
//! 生产巡检已复现（specs/prod-log-patrol-review-20261029/ws-repro.*）：
//! Pingora HTTP/1 server 的下游默认 `read_timeout=60s` 对升级后的 WS 隧道
//! 仍生效（升级帧仍经 `read_body` 逐次读取），空闲客户端在 60s 被标记
//! `Downstream ReadTimedout / reading body / 400` 截断。修复仅对 UserApp
//! app 代理路由（dev/prod）上**成功升级**（下游 Upgrade 请求 + 上游
//! 101/HTTP1.1——与 Pingora 写 101 时置 `upgraded` 的同一握手验证）的
//! 连接，把下游读设置为有限空闲窗口（默认 3600s，随每次客户端读刷新）。
//!
//! 全部用例经真实 `PingoraServerManager`（`ProxyServiceWrapper` →
//! `PortProxy`）与真实 dev/prod app 代理路由，不测 helper。dev 用例在
//! IPv6 回环 `::1` 上固定绑定 `APP_ENTRY_PORT`（9080，dev app 代理的
//! 恒定上游端口）——避开 127.0.0.1 固定端口被本机真实开发流量争用
//! （2026-10-29 实测 127.0.0.1:4224 被真实 dbx-web 轮询撞入；IPv4/IPv6
//! 回环互不相通）。
//!
//! 范围说明：非升级请求收到 101（上下游两侧一致认为非升级）由 Pingora
//! 透传 informational 响应处理，其空闲读不经过 `read_timeout`，无可诚实
//! 观测的超时断言，不单列用例；门控从三侧覆盖——"Upgrade 请求无响应
//!（后端挂起）不放宽"（non_upgraded 用例）、"Upgrade 请求被非 101 拒绝
//! 不放宽"（upgrade_denied 用例）、"成功升级才放宽"（核心反例）。

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// dev 定位桩：所有 app 指向本测试的隔离后端（**IPv6 回环 ::1** 上的
/// APP_ENTRY_PORT——避免 127.0.0.1:9080 被本机真实开发流量争用；macOS
/// lo0 未配置 127.x 别名不可绑。只定位、不创建——升级超时回归不得触碰
/// 真实计算资源）。
struct FixedDevEnsure;

#[async_trait::async_trait]
impl shared_types::UserappDevEnsure for FixedDevEnsure {
    async fn locate_dev_builder(
        &self,
        _app_id: &str,
    ) -> Result<Option<shared_types::DevBuilderInstance>, shared_types::DevEnsureError> {
        Ok(Some(shared_types::DevBuilderInstance {
            address: "::1".into(),
            container_id: "ws-timeout-fixture".into(),
        }))
    }

    async fn ensure_dev_container(
        &self,
        _app_id: &str,
    ) -> Result<shared_types::ContainerBasicInfo, shared_types::DevEnsureError> {
        panic!("ws upgrade timeout regression must not create real compute containers");
    }
}

/// 生产装配形态的运行中 wake control（prod app 代理请求过滤阶段的常备依赖）。
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

/// 以原始 TCP 完成 WebSocket 升级握手，返回已升级连接。
async fn ws_connect(port: u16, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect proxy");
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send upgrade request");
    let header = read_header_bytes(&mut stream).await;
    assert!(
        header.starts_with(b"HTTP/1.1 101"),
        "upgrade must succeed, got: {}",
        String::from_utf8_lossy(&header)
    );
    stream
}

/// 无掩码负载零长度 ping 帧（客户端→服务端，需带掩码位）。
const MASKED_PING: &[u8] = &[0x89, 0x80, 0, 0, 0, 0];
/// 服务端回的零长度 pong 帧。
const PONG: [u8; 2] = [0x8a, 0];

/// 隔离后端连接统计（接受数/存活数——重复拨号与清理断言用）。
#[derive(Default)]
struct BackendStats {
    accepted: AtomicUsize,
    live: AtomicUsize,
}

impl BackendStats {
    fn counts(&self) -> (usize, usize) {
        (
            self.accepted.load(Ordering::SeqCst),
            self.live.load(Ordering::SeqCst),
        )
    }
}

/// 隔离后端行为。
#[derive(Clone, Copy)]
enum BackendBehavior {
    /// Upgrade 请求回 101 并 echo pong；非 Upgrade 请求回 200 ok。
    EchoPong,
    /// Upgrade 请求回 101 后立即关闭上游连接（上游故障形态）。
    CloseAfterUpgrade,
    /// 读到请求头后保持静默不响应（未完成 body 的挂起形态）。
    Hang,
    /// 任何请求（含 Upgrade 请求）立即回 200 ok——升级被上游以非 101
    /// 响应拒绝的真实形态（response_filter 走 `is_upgrade==Some(false)`）。
    DenyUpgrade,
}

/// 启动行为受控的隔离后端，返回连接统计。
async fn spawn_backend(listener: TcpListener, behavior: BackendBehavior) -> Arc<BackendStats> {
    let stats = Arc::new(BackendStats::default());
    let stats_for_task = stats.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            stats_for_task.accepted.fetch_add(1, Ordering::SeqCst);
            stats_for_task.live.fetch_add(1, Ordering::SeqCst);
            let stats = stats_for_task.clone();
            tokio::spawn(async move {
                match behavior {
                    BackendBehavior::EchoPong => {
                        let header = read_header_bytes(&mut socket).await;
                        if String::from_utf8_lossy(&header)
                            .to_ascii_lowercase()
                            .contains("upgrade: websocket")
                        {
                            socket
                                .write_all(
                                    b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
                                )
                                .await
                                .expect("backend 101");
                            let mut buffer = [0u8; 128];
                            while let Ok(n) = socket.read(&mut buffer).await {
                                if n == 0 {
                                    break;
                                }
                                socket.write_all(&PONG).await.expect("backend pong");
                            }
                        } else {
                            socket
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                                .await
                                .expect("backend http");
                        }
                    }
                    BackendBehavior::CloseAfterUpgrade => {
                        let _header = read_header_bytes(&mut socket).await;
                        socket
                            .write_all(
                                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
                            )
                            .await
                            .expect("backend 101 before close");
                        drop(socket);
                    }
                    BackendBehavior::Hang => {
                        let _header = read_header_bytes(&mut socket).await;
                        // 保持连接不响应、不关闭。
                        std::future::pending::<()>().await;
                    }
                    BackendBehavior::DenyUpgrade => {
                        let _header = read_header_bytes(&mut socket).await;
                        socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .await
                            .expect("backend deny response");
                        // 等代理回收连接（保持任务存活以正确计数）。
                        let mut sink = [0u8; 64];
                        let _read = socket.read(&mut sink).await;
                    }
                }
                stats.live.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    stats
}

/// 预留并释放一个本机端口（避免启动竞态）。
fn reserve_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve proxy port");
    listener.local_addr().expect("proxy address").port()
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

/// prod 测试后端主机名：deploy-host 下注册 published 端口映射（与既有
/// prod 协议测试同模式，保证全 feature 组合下 dial 解析确定性）；默认
/// 形态直接回环。
fn prod_backend_host(port: u16) -> String {
    #[cfg(feature = "deploy-host")]
    {
        let host = format!("ws-timeout-backend-{port}");
        shared_types::published::register_port(&host, port, port);
        host
    }
    #[cfg(not(feature = "deploy-host"))]
    {
        let _ = port;
        "127.0.0.1".to_string()
    }
}

/// 用例结束时注销 prod 测试后端的 published 映射（deploy-host）。
fn prod_backend_unregister(host: &str) {
    #[cfg(feature = "deploy-host")]
    shared_types::published::unregister(host);
    #[cfg(not(feature = "deploy-host"))]
    let _ = host;
}

/// 反例（批次一核心，真实 60s 默认值证据）：dev app 代理上成功升级的
/// 空闲 WebSocket 必须**在同一条连接上**跨过下游默认 60s 读超时：空闲
/// 62s 无 EOF/数据/错误后，继续在该连接发送 ping 并确认 pong（收发
/// 恢复的见证不来自另一条连接）；另一条 20s 心跳对照连接并行提供持续
/// 收发节奏。修复前：空闲连接在 60.01s EOF，并产生与生产 492 条相同的
/// `Downstream ReadTimedout / reading body, timeout: 60s / 400` 签名。
#[tokio::test]
async fn dev_upgraded_websocket_survives_idle_past_default_downstream_timeout() {
    let backend_addr: std::net::SocketAddr = format!("[::1]:{}", shared_types::APP_ENTRY_PORT)
        .parse()
        .expect("parse IPv6 loopback backend addr");
    let backend = TcpListener::bind(backend_addr)
        .await
        .expect("isolated backend on [::1]:APP_ENTRY_PORT");
    spawn_backend(backend, BackendBehavior::EchoPong).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port));
    // deploy-host 下 dial 走 published 注册表解析——注册回环别名直连保证
    // 全 feature 组合下地址解析确定性（同 dev_recovery 既有测试模式）。
    #[cfg(feature = "deploy-host")]
    shared_types::published::register_direct(
        "::1",
        "::1".parse::<std::net::IpAddr>().expect("IPv6 loopback"),
    );
    manager.service().set_dev_ensure(Arc::new(FixedDevEnsure));
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let path = "/api/v1/userapp/proxy/app/dev/u1/wsapp/ws";
    let mut heartbeat = ws_connect(proxy_port, path).await;
    let mut idle = ws_connect(proxy_port, path).await;
    let started = Instant::now();

    // 对照连接（并行任务）：每 20s 一个 ping，跨过 60s 默认值仍持续 pong。
    let heartbeat_task = tokio::spawn(async move {
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(20)).await;
            heartbeat.write_all(MASKED_PING).await.expect("send ping");
            let mut buffer = [0u8; 2];
            tokio::time::timeout(Duration::from_secs(3), heartbeat.read_exact(&mut buffer))
                .await
                .expect("pong deadline")
                .expect("read pong");
            assert_eq!(buffer, PONG);
        }
    });

    // 同一条空闲连接：62s 内不得出现 EOF/数据/错误——跨过 60s 默认窗口。
    let mut buffer = [0u8; 16];
    match tokio::time::timeout(Duration::from_secs(62), idle.read(&mut buffer)).await {
        Err(_elapsed) => {}
        Ok(Ok(0)) => panic!("空闲升级连接在 60s 默认下游读超时内被截断（生产截断根因回归）"),
        Ok(Ok(n)) => panic!("idle upgraded connection received unexpected bytes: {n}"),
        Ok(Err(error)) => panic!("idle upgraded connection errored: {error}"),
    }
    assert!(
        started.elapsed() >= Duration::from_secs(60),
        "空闲观察必须真实跨过 60s 默认窗口，elapsed={:?}",
        started.elapsed()
    );

    // 同一条连接恢复收发：空闲跨过 60s 后 ping → pong。
    idle.write_all(MASKED_PING)
        .await
        .expect("send ping after idle");
    let mut pong = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(3), idle.read_exact(&mut pong))
        .await
        .expect("pong deadline after idle")
        .expect("read pong after idle");
    assert_eq!(pong, PONG);

    heartbeat_task.await.expect("heartbeat observation task");

    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// prod app 代理的成功升级连接同样进入有限空闲窗口，且窗口随客户端
/// 读活动刷新（测试槽压缩到 2s）：空闲连接在窗口处关闭（有限窗口、
/// 非无限保活，且不是默认 60s）；0.5s 心跳的连接存活远超窗口时长
/// （若窗口是升级起的绝对期限而非按读刷新，2s 处即死）。
#[tokio::test]
async fn prod_upgraded_websocket_window_is_finite_and_refreshed_by_reads() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind isolated backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_backend(backend_listener, BackendBehavior::EchoPong).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    manager
        .service()
        .set_upgraded_downstream_read_timeout_for_tests(Duration::from_secs(2));
    let backend_host = prod_backend_host(backend_port);
    manager
        .service()
        .add_app_backend("wsapp", backend_port, backend_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let path = "/api/v1/userapp/proxy/app/prod/u1/wsapp/ws";
    let mut heartbeat = ws_connect(proxy_port, path).await;
    let mut idle = ws_connect(proxy_port, path).await;

    // 空闲连接观测（并发运行）：有限窗口（2s+松弛）内关闭。
    let idle_task = tokio::spawn(async move {
        let mut buffer = [0u8; 16];
        match tokio::time::timeout(Duration::from_secs(8), idle.read(&mut buffer)).await {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => panic!("idle upgraded connection received unexpected bytes: {n}"),
            Ok(Err(error)) => panic!("idle upgraded connection errored: {error}"),
            Err(_) => {
                panic!("升级空闲窗口未生效：空闲连接超过 2s 窗口仍未关闭（无限保活或未套用）")
            }
        }
    });

    // 心跳连接：升级后立即以 0.5s 间隔 ping（间隔 < 2s 窗口），跨过 4s
    // （2× 窗口）仍持续 pong——若窗口是升级起的绝对期限或未按读刷新，
    // 连接在 2s 处即死。注意必须与空闲观测并发：两条连接各自独立计时。
    let beat_started = Instant::now();
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        heartbeat.write_all(MASKED_PING).await.expect("send ping");
        let mut pong = [0u8; 2];
        tokio::time::timeout(Duration::from_secs(1), heartbeat.read_exact(&mut pong))
            .await
            .expect("pong deadline within refreshed window")
            .expect("read pong");
        assert_eq!(pong, PONG);
    }
    assert!(
        beat_started.elapsed() >= Duration::from_secs(4),
        "心跳连接必须真实跨过 2× 窗口时长，elapsed={:?}",
        beat_started.elapsed()
    );
    // 空闲连接在窗口处被关闭（finite window 语义）。
    tokio::time::timeout(Duration::from_secs(10), idle_task)
        .await
        .expect("idle observation deadline")
        .expect("idle observation task");

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 未成功升级的请求不进入宽松窗口——**无响应形态**（后端挂起，升级从未
/// 发生；测试槽 1s）：无 Upgrade 头的未完成 body、带 Upgrade 头但 body
/// 声明后未发完（后端永不响应）都在 1s 后仍保持连接（默认 60s 读预算
/// 未被缩短——若升级放宽泄漏到这些形态，1s 处即被 EOF 截断）。"Upgrade
/// 请求被非 101 响应拒绝"的形态由 upgrade_denied 用例单独覆盖。
#[tokio::test]
async fn non_upgraded_requests_keep_default_downstream_read_protection() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind isolated backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_backend(backend_listener, BackendBehavior::Hang).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    manager
        .service()
        .set_upgraded_downstream_read_timeout_for_tests(Duration::from_secs(1));
    let backend_host = prod_backend_host(backend_port);
    manager
        .service()
        .add_app_backend("wsapp", backend_port, backend_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let base = "/api/v1/userapp/proxy/app/prod/u1/wsapp";

    // ① 无 Upgrade 头 + 声明 body（Content-Length: 64）未发完：保持默认
    //    读预算（Hang 后端不响应；若升级放宽泄漏到该形态，1s 处即 EOF）。
    let mut plain = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect plain");
    plain
        .write_all(
            format!("POST {base}/api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 64\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("send incomplete plain request");
    let mut buffer = [0u8; 16];
    match tokio::time::timeout(Duration::from_secs(3), plain.read(&mut buffer)).await {
        Err(_) => {}
        Ok(Ok(0)) => panic!("普通未完成 body 请求不得进入升级宽松窗口（1s 处被截断）"),
        Ok(Ok(n)) => panic!("Hang 后端不应有响应数据: {n}"),
        Ok(Err(error)) => panic!("plain request errored: {error}"),
    }

    // ② 带 Upgrade 头（升级未成功——后端永不响应 101）+ 未完成 body：
    //    同样保持默认预算。
    let mut upgrade_declared = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect upgrade-declared");
    upgrade_declared
        .write_all(
            format!(
                "GET {base}/ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nContent-Length: 64\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send incomplete upgrade-declared request");
    let mut buffer = [0u8; 16];
    match tokio::time::timeout(Duration::from_secs(3), upgrade_declared.read(&mut buffer)).await {
        Err(_) => {}
        Ok(Ok(0)) => panic!("仅有 Upgrade 头、未成功升级的请求不得进入宽松窗口（1s 处被截断）"),
        Ok(Ok(n)) => panic!("Hang 后端不应有响应数据: {n}"),
        Ok(Err(error)) => panic!("upgrade-declared request errored: {error}"),
    }

    drop(plain);
    drop(upgrade_declared);
    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 普通完整 GET 行为不变：请求完整往返、响应原样到达——修复不得破坏
/// 非升级路径。
#[tokio::test]
async fn plain_get_roundtrip_stays_intact() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind isolated backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    // EchoPong：非 Upgrade 请求回 200 ok；本用例只发普通 GET，验证非升级
    // 路径的完整往返。升级被非 101 拒绝的形态由 upgrade_denied 用例专门
    // 覆盖（引用按用例名，不依赖文件内顺序或后端形态）。
    spawn_backend(backend_listener, BackendBehavior::EchoPong).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    manager
        .service()
        .set_upgraded_downstream_read_timeout_for_tests(Duration::from_secs(1));
    let backend_host = prod_backend_host(backend_port);
    manager
        .service()
        .add_app_backend("wsapp", backend_port, backend_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    stream
        .write_all(
            b"GET /api/v1/userapp/proxy/app/prod/u1/wsapp/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("send plain GET");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("response deadline")
        .expect("read response");
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.ends_with("ok"), "{text}");

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 真实"升级被拒"分支（Codex 复审补充）：Upgrade 请求得到上游 **200 非
/// 101** 响应——response_filter 走 `is_upgrade==Some(false)`，不进入升级
/// 空闲窗口（测试槽 1s）：完整请求的 200 响应原样往返；声明 body 未发
/// 完的升级请求在响应头到达后，连接 1s 窗口后仍保持（默认 60s 读预算
/// 未被缩短——若有人改用 `is_upgrade_req()` 之类的宽松判定，这里会在
/// 1s 处 EOF）。
#[tokio::test]
async fn upgrade_denied_with_non_101_response_keeps_default_protection() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind isolated backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    let stats = spawn_backend(backend_listener, BackendBehavior::DenyUpgrade).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    manager
        .service()
        .set_upgraded_downstream_read_timeout_for_tests(Duration::from_secs(1));
    let backend_host = prod_backend_host(backend_port);
    manager
        .service()
        .add_app_backend("wsapp", backend_port, backend_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    // ① 完整 Upgrade 请求被拒：200 响应完整原样到达。
    let mut denied = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect denied");
    denied
        .write_all(
            b"GET /api/v1/userapp/proxy/app/prod/u1/wsapp/ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .expect("send denied upgrade request");
    let head = read_header_bytes(&mut denied).await;
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "被拒升级必须原样转发非 101 响应：{}",
        String::from_utf8_lossy(&head)
    );
    let mut body = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(3), denied.read_exact(&mut body))
        .await
        .expect("denied response body deadline")
        .expect("read denied response body");
    assert_eq!(&body, b"ok");
    drop(denied);

    // ② Upgrade 请求 + 声明 body（Content-Length: 64）未发完：后端立即
    //    200（response_filter 已走 Some(false) 分支），随后代理继续等
    //    待未完成 body——默认读预算保持（1s 槽下 3s 内不得 EOF）。
    let mut stalled = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect stalled");
    stalled
        .write_all(
            b"GET /api/v1/userapp/proxy/app/prod/u1/wsapp/ws2 HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nContent-Length: 64\r\n\r\n",
        )
        .await
        .expect("send stalled upgrade-declared request");
    let head = read_header_bytes(&mut stalled).await;
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "上游 200 应即时转发：{}",
        String::from_utf8_lossy(&head)
    );
    let mut body = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(3), stalled.read_exact(&mut body))
        .await
        .expect("stalled response body deadline")
        .expect("read stalled response body");
    assert_eq!(&body, b"ok");
    // 响应已完整到达、body 仍未发完：默认读预算保持（1s 槽下 3s 内不得 EOF）。
    let mut buffer = [0u8; 16];
    match tokio::time::timeout(Duration::from_secs(3), stalled.read(&mut buffer)).await {
        Err(_still_open) => {}
        Ok(Ok(0)) => panic!("升级被非 101 拒绝后的未完成 body 不得进入升级宽松窗口（1s 处被截断）"),
        Ok(Ok(n)) => panic!("响应已完整消费，不应再有数据: {n}"),
        Ok(Err(error)) => panic!("stalled connection errored: {error}"),
    }

    let (accepted, _live) = stats.counts();
    assert_eq!(accepted, 2, "两条客户端连接不得产生重复上游拨号");
    drop(stalled);
    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 升级连接的两端关闭与清理：客户端主动关闭 → 上游连接随之回收（后端
/// 存活数归零）；上游升级后关闭 → 客户端及时收到 EOF；无重复上游拨号
/// （后端接受数 == 客户端连接数），活跃指标归零，代理受控退出。
#[tokio::test]
async fn upgraded_connection_closes_clean_up_both_ends_without_leaks() {
    let echo_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind echo backend");
    let echo_port = echo_listener.local_addr().expect("echo addr").port();
    let echo_stats = spawn_backend(echo_listener, BackendBehavior::EchoPong).await;
    let close_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind close backend");
    let close_port = close_listener.local_addr().expect("close addr").port();
    let close_stats = spawn_backend(close_listener, BackendBehavior::CloseAfterUpgrade).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    let metrics = manager.service().metrics.clone();
    let echo_host = prod_backend_host(echo_port);
    let close_host = prod_backend_host(close_port);
    manager
        .service()
        .add_app_backend("wsapp", echo_port, echo_host.clone());
    manager
        .service()
        .add_app_backend("wsclose", close_port, close_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    // ① 客户端升级后主动关闭 → 代理回收上游连接。
    let client_closed = ws_connect(proxy_port, "/api/v1/userapp/proxy/app/prod/u1/wsapp/ws").await;
    drop(client_closed);
    let deadline = Instant::now() + Duration::from_secs(5);
    while echo_stats.counts().1 != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(echo_stats.counts().1, 0, "客户端关闭后上游连接必须被回收");
    assert_eq!(echo_stats.counts().0, 1, "不得重复拨号上游");

    // ② 上游升级后关闭 → 客户端及时收到 EOF。
    let mut upstream_closed =
        ws_connect(proxy_port, "/api/v1/userapp/proxy/app/prod/u1/wsclose/ws").await;
    let mut buffer = [0u8; 16];
    match tokio::time::timeout(Duration::from_secs(5), upstream_closed.read(&mut buffer)).await {
        Ok(Ok(0)) => {}
        Ok(Ok(n)) => panic!("upstream-closed connection received unexpected bytes: {n}"),
        Ok(Err(error)) => panic!("upstream-closed connection errored: {error}"),
        Err(_) => panic!("上游关闭必须及时传播到客户端（5s 内 EOF）"),
    }
    assert_eq!(close_stats.counts().0, 1, "不得重复拨号上游");
    let deadline = Instant::now() + Duration::from_secs(5);
    while close_stats.counts().1 != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(close_stats.counts().1, 0, "后端任务必须全部退出");

    // ③ 活跃指标归零。
    let deadline = Instant::now() + Duration::from_secs(5);
    while metrics.active() != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(metrics.active(), 0, "活跃连接指标必须归零");

    prod_backend_unregister(&echo_host);
    prod_backend_unregister(&close_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}
