//! 有界单对象观察的流式 HTTP 契约测试（kube-runtime 批次 A，KR01–KR06）。
//!
//! 受控 HTTP 流式服务器提供真实 kube wire 协议（LIST 初始快照 + WATCH 事件
//! 流），按场景断言：正常就绪、错误分类（403）、deadline、取消、事件拆分。
//! 不依赖随机 sleep——推进时机由服务器主动写事件驱动。

use std::time::{Duration, Instant};

use kube::api::ListParams;
use tokio::io::AsyncWriteExt as _;

use super::{ObservationError, Verdict, await_pod_verdict, classify_pod_readiness};

/// 极简受控 apiserver：按预设脚本应答 GET（LIST）与后续 WATCH 流写入。
struct ScriptedApiServer {
    listener: tokio::net::TcpListener,
    /// LIST 应答 JSON（完整 PodList）。
    list_body: String,
    /// WATCH 阶段向连接写入的 chunk 列表（每项一次 write_all + flush）。
    watch_chunks: Vec<String>,
    /// LIST 应答状态码（403 场景）。
    list_status: u16,
}

impl ScriptedApiServer {
    async fn start(
        list_body: String,
        watch_chunks: Vec<String>,
        list_status: u16,
    ) -> anyhow::Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            // watcher 依次发起 LIST 与 WATCH 两条连接：循环应答直至脚本耗尽
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut text = String::new();
                let mut buffer = [0u8; 4096];
                while !text.contains("\r\n\r\n") {
                    let n = stream.read(&mut buffer).await.expect("read head");
                    if n == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buffer[..n]));
                }
                let is_watch = text.contains("watch=true");
                if is_watch {
                    // WATCH：chunked 流式应答
                    let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
                    stream
                        .write_all(header.as_bytes())
                        .await
                        .expect("watch header");
                    for chunk in &watch_chunks {
                        let size = format!("{:x}\r\n", chunk.len());
                        stream.write_all(size.as_bytes()).await.expect("size");
                        stream.write_all(chunk.as_bytes()).await.expect("chunk");
                        stream.write_all(b"\r\n").await.expect("crlf");
                        stream.flush().await.expect("flush");
                    }
                    // 保持连接打开直至对端断开（观察结束）；断开后回到 accept
                    let mut sink = stream;
                    let mut drain = [0u8; 1024];
                    loop {
                        use tokio::io::AsyncReadExt as _;
                        if sink.read(&mut drain).await.unwrap_or(0) == 0 {
                            break;
                        }
                    }
                } else {
                    // LIST：一次性 JSON 应答
                    let status_line = format!(
                        "HTTP/1.1 {list_status} {}\r\n",
                        if list_status == 200 {
                            "OK"
                        } else {
                            "Forbidden"
                        }
                    );
                    let body = if list_status == 200 {
                        list_body.clone()
                    } else {
                        serde_json::json!({
                            "kind": "Status", "apiVersion": "v1", "status": "Failure",
                            "message": "pods is forbidden: User \"test\" cannot list resource",
                            "reason": "Forbidden", "code": 403
                        })
                        .to_string()
                    };
                    let header = format!(
                        "{status_line}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream
                        .write_all(header.as_bytes())
                        .await
                        .expect("list header");
                    stream.write_all(body.as_bytes()).await.expect("list body");
                }
            }
        });
        Ok((address, server))
    }

    async fn client(address: std::net::SocketAddr) -> kube::Client {
        drop(rustls::crypto::ring::default_provider().install_default());
        let config = kube::Config::new(format!("http://{address}").parse().expect("URI"));
        kube::Client::try_from(config).expect("client")
    }
}

fn pod_json(ready: bool) -> String {
    serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "watch-pod", "namespace": "default", "uid": "uid-1", "resourceVersion": "1"},
        "spec": {"containers": [{"name": "app", "image": "fixture"}]},
        "status": {
            "phase": "Running",
            "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]
        }
    })
    .to_string()
}

fn list_body() -> String {
    serde_json::json!({
        "apiVersion": "v1", "kind": "PodList",
        "metadata": {"resourceVersion": "10"},
        "items": [serde_json::from_str::<serde_json::Value>(&pod_json(false)).expect("pod")]
    })
    .to_string()
}

fn watch_event(event_type: &str, pod: &str) -> String {
    let object: serde_json::Value = serde_json::from_str(pod).expect("pod json");
    serde_json::json!({"type": event_type, "object": object}).to_string()
}

async fn observe(
    address: std::net::SocketAddr,
    deadline: Instant,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<Verdict<()>, ObservationError> {
    let client = ScriptedApiServer::client(address).await;
    let api: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::default_namespaced(client);
    await_pod_verdict(&api, "watch-pod", deadline, cancel, classify_pod_readiness).await
}

/// 正常路径：LIST 未就绪 + WATCH Modified 就绪 → Complete。
#[tokio::test]
async fn watch_stream_reaches_ready_verdict() {
    let ready_pod = pod_json(true);
    let (address, server) = ScriptedApiServer::start(
        list_body(),
        vec![format!("{}\n", watch_event("MODIFIED", &ready_pod))],
        200,
    )
    .await
    .expect("server");
    let deadline = Instant::now() + Duration::from_secs(10);
    let verdict = observe(
        address,
        deadline,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("verdict");
    assert!(matches!(verdict, Verdict::Complete(())));
    server.abort();
}

/// LIST 403 → Fatal（保留 API code，不退化成超时——KR05）。
#[tokio::test]
async fn forbidden_list_fails_fast_with_api_code() {
    let (address, server) = ScriptedApiServer::start(String::new(), vec![], 403)
        .await
        .expect("server");
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = observe(
        address,
        deadline,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect_err("must fail");
    match &error {
        ObservationError::Fatal { code, .. } => assert_eq!(*code, Some(403)),
        other => panic!("expected Fatal(403), got {other:?}"),
    }
    server.abort();
}

/// 总 deadline：无判定事件到达 → Deadline（嵌套不重置预算——KR03）。
#[tokio::test]
async fn total_deadline_bounds_silent_stream() {
    let (address, server) = ScriptedApiServer::start(list_body(), vec![], 200)
        .await
        .expect("server");
    let deadline = Instant::now() + Duration::from_millis(300);
    let error = observe(
        address,
        deadline,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect_err("must time out");
    assert!(matches!(error, ObservationError::Deadline { .. }));
    server.abort();
}

/// 取消：观察中的流被释放（KR04——不遗留后台任务）。
#[tokio::test]
async fn cancellation_releases_observation() {
    let (address, server) = ScriptedApiServer::start(list_body(), vec![], 200)
        .await
        .expect("server");
    let cancel = tokio_util::sync::CancellationToken::new();
    let observer = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let deadline = Instant::now() + Duration::from_secs(30);
            observe(address, deadline, cancel).await
        }
    });
    // 给观察建立流的时间（LIST 往返完成即认为已挂上 WATCH）
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), observer)
        .await
        .expect("cancelled observer must finish promptly")
        .expect("observer task");
    assert!(matches!(result, Err(ObservationError::Cancelled)));
    server.abort();
}

/// 拆分事件：单 chunk 内两条事件（未就绪→就绪）依序消费，终态取最后判定。
#[tokio::test]
async fn split_events_in_one_chunk_are_consumed_in_order() {
    let pending_event = watch_event("MODIFIED", &pod_json(false));
    let ready_event = watch_event("MODIFIED", &pod_json(true));
    let chunk = format!("{pending_event}\n{ready_event}\n");
    let (address, server) = ScriptedApiServer::start(list_body(), vec![chunk], 200)
        .await
        .expect("server");
    let deadline = Instant::now() + Duration::from_secs(10);
    let verdict = observe(
        address,
        deadline,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("verdict");
    assert!(matches!(verdict, Verdict::Complete(())));
    server.abort();
}

/// 事件分类契约锁：LIST 只返回 Pending（未就绪不算成功）——KR01。
#[tokio::test]
async fn pending_only_list_does_not_complete() {
    let (address, server) = ScriptedApiServer::start(list_body(), vec![], 200)
        .await
        .expect("server");
    let deadline = Instant::now() + Duration::from_millis(300);
    let error = observe(
        address,
        deadline,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect_err("pending must not complete");
    assert!(matches!(error, ObservationError::Deadline { .. }));
    server.abort();
    // 显式使用 ListParams 防止误删导入（观察契约走 watcher::Config）。
    let _ = ListParams::default();
}
