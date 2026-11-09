//! 批次二A回归：预期客户端断开的日志分类（真实 ProxyServiceWrapper）。
//!
//! 生产巡检证据：431 条 4224 端口（dbx 流式接口）`Downstream
//! ConnectionClosed` ERROR——客户端主动断开流式连接属预期行为，不应以
//! 平台 ERROR 重复告警。分类条件（全部满足才降级）：
//! ① 已知流式路由（UserApp app dev/prod 预览 或 dbx 族，request_filter
//!    阶段捕获 `TrackingCtx::streaming_route`）；
//! ② 响应已建立（`response_written()` 为 200+ 或 101——与错误页守卫
//!    同语义）；
//! ③ 错误源为 Downstream 且 etype 为 ConnectionClosed（客户端 FIN）。
//! 降级为 debug 常规 + 首条/每 100 条采样 info（有界 counter：
//! `ProxyMetrics::client_disconnected_suppressed`），URI 按请求原文记录，保留临时凭据，便于排查。
//!
//! 不降级（保持 ERROR）：Upstream 错误、其他 etype（含 ReadTimedout——
//! 下游空闲截断是批次一已根因修复的缺陷签名，不得藏进静默）、响应未
//! 建立前的断开、非流式路由。全部用例经真实 `PingoraServerManager`
//! （ProxyServiceWrapper → PortProxy），用全局 tracing subscriber 捕获
//! pingora 线程的实际日志输出断言。

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// dev 定位桩：所有 app 指向本测试的隔离后端（**IPv6 回环 ::1** 上的
/// DBX_PORT——127.0.0.1:4224 会被本机真实 dbx-web 轮询流量撞入
/// （2026-10-29 实测），IPv4/IPv6 回环互不相通；127.0.0.3 等别名在
/// macOS lo0 未配置不可绑定）。
struct FixedDevEnsure;

#[async_trait::async_trait]
impl shared_types::UserappDevEnsure for FixedDevEnsure {
    async fn locate_dev_builder(
        &self,
        _app_id: &str,
    ) -> Result<Option<shared_types::DevBuilderInstance>, shared_types::DevEnsureError> {
        Ok(Some(shared_types::DevBuilderInstance {
            address: "::1".into(),
            container_id: "client-disconnect-fixture".into(),
        }))
    }

    async fn ensure_dev_container(
        &self,
        _app_id: &str,
    ) -> Result<shared_types::ContainerBasicInfo, shared_types::DevEnsureError> {
        panic!("client disconnect regression must not create real compute containers");
    }
}

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
        Duration::from_secs(2)
    }
}

/// 进程级共享日志缓冲（nextest 每用例独立进程，全局 subscriber 可用）。
#[derive(Clone)]
struct SharedLogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedLogWriter {
    fn install() -> Self {
        let writer = Self(Arc::default());
        // 按生产日志级别（INFO）捕获：pingora 内部 DEBUG 会整段回显上游
        // 请求头（含 query），生产不启用该级别，不在本断言范围。
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("install per-test log capture");
        // pingora 内部 error!/warn! 走 log 门面——装桥才能进入捕获缓冲。
        tracing_log::LogTracer::init().expect("bridge log facade into tracing");
        writer
    }

    fn contains(&self, needle: &str) -> bool {
        let buffer = self.0.lock().expect("log buffer lock");
        String::from_utf8_lossy(&buffer).contains(needle)
    }

    fn text(&self) -> String {
        let buffer = self.0.lock().expect("log buffer lock");
        String::from_utf8_lossy(&buffer).into_owned()
    }
}

impl std::io::Write for SharedLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogWriter {
    type Writer = SharedLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// 隔离后端行为。
#[derive(Clone, Copy)]
enum BackendBehavior {
    /// 读到请求头后保持静默不响应、不关闭。
    Hang,
    /// 回 200 + chunked 头 + 一个数据块后挂起（响应已建立、body 未完）。
    PartialChunked,
}

/// 启动行为受控后端，返回存活连接计数。每条连接任务以**观察到上游
/// EOF/读错误**为退出见证（代理回收上游连接时触发），存活归零=上游
/// 连接确实被回收——不是只看代理侧响应阶段指标。
async fn spawn_backend(listener: TcpListener, behavior: BackendBehavior) -> Arc<AtomicUsize> {
    let live = Arc::new(AtomicUsize::new(0));
    let live_task = live.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            live_task.fetch_add(1, Ordering::SeqCst);
            let live = live_task.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                loop {
                    let mut byte = [0u8; 1];
                    match socket.read(&mut byte).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            buffer.push(byte[0]);
                            if buffer.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                match behavior {
                    BackendBehavior::Hang => {
                        // 不响应；持续读直到代理回收连接（EOF 见证）。
                        read_until_eof(&mut socket).await;
                    }
                    BackendBehavior::PartialChunked => {
                        let _write = socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nA\r\n",
                            )
                            .await;
                        read_until_eof(&mut socket).await;
                    }
                }
                live.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    live
}

/// 读至 EOF/错误——返回即代表对端关闭了连接（上游回收的真实见证）。
async fn read_until_eof(socket: &mut TcpStream) {
    let mut sink = [0u8; 256];
    loop {
        match socket.read(&mut sink).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// 等待并断言上游连接被代理回收（后端侧 EOF 见证）；超时未归零即失败。
async fn wait_upstream_reclaimed(live: &AtomicUsize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while live.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        live.load(Ordering::SeqCst),
        0,
        "上游连接必须在期限内被代理回收（后端侧 EOF 见证），否则清理不成立"
    );
}

/// 排空客户端接收缓冲后关闭——保证关闭发 FIN（→ ConnectionClosed，与
/// 生产 431 条签名一致）而非 RST（未读数据触发 ReadError，不属本分类）。
async fn drain_then_close(mut stream: TcpStream) {
    let mut sink = [0u8; 256];
    while matches!(
        tokio::time::timeout(Duration::from_millis(150), stream.read(&mut sink)).await,
        Ok(Ok(n)) if n > 0
    ) {}
    drop(stream);
}

fn reserve_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve proxy port");
    listener.local_addr().expect("proxy address").port()
}

async fn wait_listening(port: u16) {
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("isolated proxy never listened on {port}");
}

fn prod_backend_host(port: u16) -> String {
    #[cfg(feature = "deploy-host")]
    {
        let host = format!("client-disconnect-backend-{port}");
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

/// 反例（431 条签名的主形态）：dbx 流式路由上**响应已建立**（200+chunked
/// 分块下发中）客户端主动断开，不得再产生平台 `Fail to proxy` ERROR；
/// 取而代之的是降级分类行（完整保留原请求 URI）与有界
/// counter 递增。注：升级 WS 隧道的客户端 FIN 在 pingora 0.9 是优雅
/// 结束（proxy_h1.rs:592 body None + was_upgraded → upstream done），
/// 生产签名来自非升级流式响应的中途断开，本用例按该形态构造。
#[tokio::test]
async fn dbx_streaming_client_disconnect_is_downgraded_not_error() {
    let logs = SharedLogWriter::install();
    let backend_addr: std::net::SocketAddr = format!("[::1]:{}", shared_types::DBX_PORT)
        .parse()
        .expect("parse IPv6 loopback backend addr");
    let backend = TcpListener::bind(backend_addr)
        .await
        .expect("isolated backend on [::1]:DBX_PORT");
    let live = spawn_backend(backend, BackendBehavior::PartialChunked).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port));
    #[cfg(feature = "deploy-host")]
    shared_types::published::register_direct(
        "::1",
        "::1".parse::<std::net::IpAddr>().expect("IPv6 loopback"),
    );
    manager.service().set_dev_ensure(Arc::new(FixedDevEnsure));
    let metrics = manager.service().metrics.clone();
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    client
        .write_all(
            b"GET /api/v1/userapp/proxy/dbx/dev/u1/dbxapp/api/stream?token=secret-dbix-token&%74oken=encoded-temp-token&password=temp-pass&x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .expect("send streaming request");
    // 读到响应头与首个数据块（响应已建立），随后主动断开。
    let mut buffer = [0u8; 128];
    let read = tokio::time::timeout(Duration::from_secs(3), client.read(&mut buffer))
        .await
        .expect("read deadline")
        .expect("read response head");
    assert!(read > 0, "must receive response head");
    drain_then_close(client).await;
    wait_upstream_reclaimed(&live).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        !logs.contains("Fail to proxy"),
        "预期客户端断开不得再报平台 ERROR，实际日志：\n{}",
        logs.text()
    );
    assert_eq!(
        metrics
            .client_disconnected_suppressed
            .load(Ordering::SeqCst),
        1,
        "有界 counter 必须记录一次降级分类"
    );
    let text = logs.text();
    assert!(
        text.contains("client_disconnected"),
        "降级行必须带分类标记：\n{text}"
    );
    assert!(
        text.contains("/api/v1/userapp/proxy/dbx/dev/u1/dbxapp/api/stream?token=secret-dbix-token&%74oken=encoded-temp-token&password=temp-pass&x=1"),
        "INFO 采样必须保留完整原始 URI，包括临时凭据与编码参数名：\n{text}"
    );

    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 同分类覆盖 app 预览路由：响应已建立的流式请求中途客户端断开同样降级。
#[tokio::test]
async fn app_streaming_client_disconnect_is_downgraded_not_error() {
    let logs = SharedLogWriter::install();
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind isolated backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    let live = spawn_backend(backend_listener, BackendBehavior::PartialChunked).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    let backend_host = prod_backend_host(backend_port);
    manager
        .service()
        .add_app_backend("wsapp", backend_port, backend_host.clone());
    let metrics = manager.service().metrics.clone();
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    client
        .write_all(
            b"GET /api/v1/userapp/proxy/app/prod/u1/wsapp/api/events HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .expect("send streaming request");
    let mut buffer = [0u8; 128];
    let read = tokio::time::timeout(Duration::from_secs(3), client.read(&mut buffer))
        .await
        .expect("read deadline")
        .expect("read response head");
    assert!(read > 0, "must receive response head");
    drain_then_close(client).await;
    wait_upstream_reclaimed(&live).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        !logs.contains("Fail to proxy"),
        "预期客户端断开不得再报平台 ERROR，实际日志：\n{}",
        logs.text()
    );
    assert_eq!(
        metrics
            .client_disconnected_suppressed
            .load(Ordering::SeqCst),
        1,
        "有界 counter 必须记录一次降级分类"
    );
    assert!(logs.contains("client_disconnected"), "{}", logs.text());

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 守卫：同一流式路由上的上游建连失败必须保持 ERROR 可诊断（不因分类
/// 静默 Upstream 错误）。
#[tokio::test]
async fn upstream_connect_failure_on_streaming_route_still_logs_error() {
    let logs = SharedLogWriter::install();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve dead port");
    let dead_port = reservation.local_addr().expect("addr").port();
    drop(reservation);

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    let backend_host = prod_backend_host(dead_port);
    manager
        .service()
        .add_app_backend("deadapp", dead_port, backend_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    client
        .write_all(
            b"GET /api/v1/userapp/proxy/app/prod/u1/deadapp/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("send request");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), client.read_to_end(&mut response))
        .await
        .expect("response deadline")
        .expect("read response");
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        logs.contains("Fail to proxy"),
        "上游失败必须保留 ERROR 日志，实际：\n{}",
        logs.text()
    );

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 守卫：响应未建立前的客户端断开（未拿到任何响应头）不进入降级分类——
/// 保持 ERROR（未建连/未响应就断开不是预期流式断开形态）。
#[tokio::test]
async fn disconnect_before_response_established_still_logs_error() {
    let logs = SharedLogWriter::install();
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind isolated backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_backend(backend_listener, BackendBehavior::Hang).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
        .with_wake_control(Arc::new(RunningWakeControl));
    let backend_host = prod_backend_host(backend_port);
    manager
        .service()
        .add_app_backend("hangapp", backend_port, backend_host.clone());
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    client
        .write_all(
            b"GET /api/v1/userapp/proxy/app/prod/u1/hangapp/stream HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .expect("send request");
    tokio::time::sleep(Duration::from_millis(400)).await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(
        logs.contains("Fail to proxy"),
        "响应未建立前的断开必须保持 ERROR，实际：\n{}",
        logs.text()
    );

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}

/// 守卫：非流式路由（/proxy/{port}）响应中途客户端断开——同一错误类别
/// （Downstream ConnectionClosed）也不得被分类降级。
#[tokio::test]
async fn non_streaming_route_disconnect_still_logs_error() {
    let logs = SharedLogWriter::install();
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind isolated backend");
    let backend_port = backend_listener.local_addr().expect("backend addr").port();
    spawn_backend(backend_listener, BackendBehavior::PartialChunked).await;

    let proxy_port = reserve_port();
    let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port));
    let backend_host = prod_backend_host(backend_port);
    manager
        .service()
        .add_backend(backend_port, backend_host.clone())
        .await;
    let (stop, shutdown_rx) = oneshot::channel();
    let proxy_task = tokio::spawn(async move { manager.start(shutdown_rx).await.unwrap() });
    wait_listening(proxy_port).await;

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .expect("connect proxy");
    client
        .write_all(
            format!("GET /proxy/{backend_port}/x HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
        )
        .await
        .expect("send request");
    // 读到响应头与首个数据块（响应已建立），随后主动断开。
    let mut buffer = [0u8; 128];
    let read = tokio::time::timeout(Duration::from_secs(3), client.read(&mut buffer))
        .await
        .expect("read deadline")
        .expect("read response head");
    assert!(read > 0, "must receive response head");
    drop(client);
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(
        logs.contains("Fail to proxy"),
        "非流式路由的响应中断开必须保持 ERROR，实际：\n{}",
        logs.text()
    );

    prod_backend_unregister(&backend_host);
    stop.send(()).expect("stop isolated proxy");
    tokio::time::timeout(Duration::from_secs(10), proxy_task)
        .await
        .expect("proxy shutdown deadline")
        .expect("join proxy task");
}
