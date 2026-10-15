use super::*;
#[cfg(feature = "proxy")]
use crate::config::ProxyConfig;
#[cfg(feature = "proxy")]
use std::net::TcpListener;
#[cfg(feature = "proxy")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(feature = "proxy")]
struct ProxyObserver {
    server_task: tokio::task::AbortHandle,
    health_stop: tokio::sync::watch::Sender<bool>,
    health_task: Option<tokio::task::AbortHandle>,
}

#[cfg(feature = "proxy")]
impl ProxyObserver {
    async fn new(handle: &HttpServerHandle) -> Self {
        let proxy = handle.pingora_result.lock().await;
        let (server_task, health_stop, health_task) = proxy
            .as_ref()
            .expect("observe the original embedded proxy")
            .shutdown_observer();
        Self {
            server_task,
            health_stop,
            health_task,
        }
    }

    async fn cleanup(&self) {
        tokio::time::timeout(Duration::from_secs(12), async {
            while !self.server_task.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("original proxy task cleanup deadline");
        self.health_stop.send_replace(true);
        if let Some(task) = &self.health_task {
            tokio::time::timeout(Duration::from_secs(2), async {
                while !task.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("original health task cleanup deadline");
        }
    }
}

#[derive(Debug)]
#[cfg(feature = "proxy")]
struct ExitAtReturn {
    http_listener_released: bool,
    proxy_listener_released: bool,
    http_tasks_joined: bool,
    original_proxy_task_finished: bool,
    original_health_task_finished: bool,
}

#[cfg(feature = "proxy")]
impl ExitAtReturn {
    // 在 stop 返回后的第一次 await 前获取快照；第二个 stop 不能替第一个完成清理。
    fn capture(
        handle: &HttpServerHandle,
        observer: &ProxyObserver,
        http_port: u16,
        proxy_port: u16,
    ) -> Self {
        Self {
            http_listener_released: TcpListener::bind(("0.0.0.0", http_port)).is_ok(),
            proxy_listener_released: TcpListener::bind(("0.0.0.0", proxy_port)).is_ok(),
            http_tasks_joined: handle
                .join_set
                .try_lock()
                .is_ok_and(|tasks| tasks.is_empty()),
            original_proxy_task_finished: observer.server_task.is_finished(),
            original_health_task_finished: observer
                .health_task
                .as_ref()
                .is_none_or(tokio::task::AbortHandle::is_finished),
        }
    }

    fn all_exited(&self) -> bool {
        self.http_listener_released
            && self.proxy_listener_released
            && self.http_tasks_joined
            && self.original_proxy_task_finished
            && self.original_health_task_finished
    }
}

async fn poll_stop_once(
    handle: &HttpServerHandle,
) -> std::pin::Pin<Box<impl Future<Output = Result<()>> + '_>> {
    let mut stop = Box::pin(handle.stop());
    // current-thread 中一次 poll 已受理取消并进入真实 join；不让后台服务推进。
    tokio::select! {
        biased;
        result = &mut stop => panic!("first stop unexpectedly completed: {result:?}"),
        () = std::future::ready(()) => {}
    }
    stop
}

fn controlled_http_handle(tasks: JoinSet<Result<()>>) -> HttpServerHandle {
    HttpServerHandle {
        shutdown_token: CancellationToken::new(),
        join_set: Arc::new(tokio::sync::Mutex::new(tasks)),
        shutdown_state: Arc::new(tokio::sync::Mutex::new(HttpShutdownState::default())),
        #[cfg(feature = "proxy")]
        pingora_result: Arc::new(tokio::sync::Mutex::new(None)),
    }
}

#[cfg(feature = "proxy")]
async fn start_test_http_server() -> (HttpServerHandle, u16, u16, tempfile::TempDir) {
    let http_reservation = TcpListener::bind("127.0.0.1:0").expect("reserve HTTP port");
    let proxy_reservation = TcpListener::bind("127.0.0.1:0").expect("reserve proxy port");
    let http_port = http_reservation.local_addr().expect("HTTP address").port();
    let proxy_port = proxy_reservation
        .local_addr()
        .expect("proxy address")
        .port();
    drop(http_reservation);
    drop(proxy_reservation);
    let workspace = tempfile::tempdir().expect("test workspace");
    let config = HttpServerConfig {
        port: http_port,
        app_config: AppConfig {
            port: http_port,
            projects_dir: workspace.path().to_path_buf(),
            proxy_config: Some(ProxyConfig {
                listen_port: proxy_port,
                default_backend_port: http_port,
                ..Default::default()
            }),
            ..Default::default()
        },
        agent_session_service: Arc::new(AgentSessionService::new(
            agent_abstraction::launcher::direct_model_runtime_env_resolver(),
            100,
        )),
        shared_api_key_manager: Arc::new(dashmap::DashMap::new()),
        project_uuid_map: None,
        agent_mgmt_registry: None,
        agent_mgmt_path_manager: None,
    };
    let handle = start_http_server(config)
        .await
        .expect("start HTTP with proxy");
    let listening = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if listening.is_err() {
        handle.stop().await.expect("cleanup unready HTTP server");
        panic!("real embedded proxy never listened");
    }
    (handle, http_port, proxy_port, workspace)
}

#[tokio::test]
#[cfg(feature = "proxy")]
async fn concurrent_http_stop_waits_for_real_proxy_and_http_exit() {
    let (handle, http_port, proxy_port, _workspace) = start_test_http_server().await;
    let observer = ProxyObserver::new(&handle).await;
    let mut connection = tokio::net::TcpStream::connect(("127.0.0.1", http_port))
        .await
        .expect("connect actual HTTP server");
    connection
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("send actual HTTP request");
    let mut response = Vec::new();
    let response_result = tokio::time::timeout(
        Duration::from_secs(5),
        connection.read_to_end(&mut response),
    )
    .await;
    let other_handle = handle.clone();
    let (first, second) = tokio::join!(
        async {
            let result = handle.stop().await;
            let at_return = ExitAtReturn::capture(&handle, &observer, http_port, proxy_port);
            (result, at_return)
        },
        async {
            let result = other_handle.stop().await;
            let at_return = ExitAtReturn::capture(&other_handle, &observer, http_port, proxy_port);
            (result, at_return)
        }
    );
    observer.cleanup().await;
    let (first, first_exit) = first;
    let (second, second_exit) = second;
    first.expect("first HTTP shutdown");
    second.expect("concurrent HTTP shutdown");
    response_result
        .expect("actual HTTP response deadline")
        .expect("actual HTTP response");
    assert!(response.starts_with(b"HTTP/1.1 "));
    assert!(first_exit.all_exited(), "first return: {first_exit:?}");
    assert!(second_exit.all_exited(), "second return: {second_exit:?}");
    other_handle.stop().await.expect("repeat HTTP shutdown");
}

#[tokio::test]
#[cfg(feature = "proxy")]
async fn cancelled_http_stop_retains_original_proxy_until_repeated_stop_confirms_exit() {
    let (handle, http_port, proxy_port, _workspace) = start_test_http_server().await;
    let observer = ProxyObserver::new(&handle).await;
    let other_handle = handle.clone();
    let first_stop = poll_stop_once(&handle).await;
    let cancellation_was_accepted = handle.is_shutdown();
    let original_task_was_pending = !observer.server_task.is_finished();
    drop(first_stop);

    let repeated = other_handle.stop().await;
    let repeated_exit = ExitAtReturn::capture(&other_handle, &observer, http_port, proxy_port);
    // 修复前 server JoinHandle 已 detach；用原 AbortHandle 等待真实收尾后再断言。
    observer.cleanup().await;

    assert!(cancellation_was_accepted && original_task_was_pending);
    repeated.expect("repeated stop must confirm the original proxy exit");
    assert!(
        repeated_exit.all_exited(),
        "repeated stop returned before original services exited: {repeated_exit:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn cancelled_http_stop_preserves_already_consumed_task_failure() {
    let mut tasks = JoinSet::new();
    let failed = tasks.spawn(async { anyhow::bail!("consumed before cancellation") });
    tasks.spawn(std::future::pending::<Result<()>>());
    while !failed.is_finished() {
        tokio::task::yield_now().await;
    }
    let handle = controlled_http_handle(tasks);
    let first_stop = poll_stop_once(&handle).await;
    drop(first_stop);
    let remaining_after_cancel = handle.join_set.lock().await.len();
    let repeated = handle.clone().stop().await;
    let repeated_again = handle.stop().await;
    let cleanup_confirmed = handle.join_set.lock().await.is_empty();

    assert_eq!(
        remaining_after_cancel, 1,
        "first stop consumed the failing task"
    );
    assert!(cleanup_confirmed, "pending task must be actually joined");
    let error = repeated.expect_err("consumed task failure must survive caller cancellation");
    assert!(error.to_string().contains("consumed before cancellation"));
    assert_eq!(
        repeated_again
            .expect_err("consumed failure must remain sticky")
            .to_string(),
        error.to_string()
    );
}

#[tokio::test(start_paused = true)]
async fn cancelled_http_stop_keeps_first_grace_deadline() {
    struct RecordExit(Option<tokio::sync::oneshot::Sender<tokio::time::Instant>>);
    impl Drop for RecordExit {
        fn drop(&mut self) {
            if let Some(exit) = self.0.take() {
                let _ = exit.send(tokio::time::Instant::now());
            }
        }
    }
    let (entered, started) = tokio::sync::oneshot::channel();
    let (exit, exited) = tokio::sync::oneshot::channel();
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let _record_exit = RecordExit(Some(exit));
        entered.send(()).expect("record blocked HTTP task startup");
        std::future::pending::<Result<()>>().await
    });
    started.await.expect("HTTP task started");
    let handle = controlled_http_handle(tasks);
    let first_started = tokio::time::Instant::now();
    let first_stop = poll_stop_once(&handle).await;
    drop(first_stop);
    tokio::time::advance(Duration::from_secs(2)).await;
    let repeated = handle.clone().stop().await;
    let actual_exit = exited.await.expect("observe actual HTTP task cancellation");
    let cleanup_confirmed = handle.join_set.lock().await.is_empty();

    repeated.expect("repeated shutdown completes");
    assert!(cleanup_confirmed);
    assert!(
        actual_exit <= first_started + Duration::from_millis(3010),
        "retry extended initial HTTP grace: elapsed={:?}",
        actual_exit.duration_since(first_started)
    );
}

#[tokio::test(start_paused = true)]
async fn cancelled_http_stop_after_abort_resumes_drain_without_false_task_failure() {
    let mut tasks = JoinSet::new();
    let original_task = tasks.spawn(std::future::pending::<Result<()>>());
    let handle = controlled_http_handle(tasks);
    let mut first_stop = poll_stop_once(&handle).await;
    tokio::time::advance(Duration::from_millis(3010)).await;
    // 在 current-thread 中第二次 poll 发出 abort，但尚未让原任务处理取消。
    tokio::select! {
        biased;
        result = &mut first_stop => panic!("stop completed before aborted task ran: {result:?}"),
        () = std::future::ready(()) => {}
    }
    let task_still_pending_when_cancelled = !original_task.is_finished();
    drop(first_stop);
    let abort_phase_retained = handle
        .shutdown_state
        .try_lock()
        .is_ok_and(|state| state.http_abort_requested);

    let repeated = handle.clone().stop().await;
    let original_task_finished_at_return = original_task.is_finished();
    let tasks_joined_at_return = handle
        .join_set
        .try_lock()
        .is_ok_and(|tasks| tasks.is_empty());

    assert!(task_still_pending_when_cancelled && abort_phase_retained);
    repeated.expect("own abort result must remain a successful confirmed shutdown");
    assert!(original_task_finished_at_return && tasks_joined_at_return);
}

#[tokio::test]
#[cfg(feature = "proxy")]
async fn http_shutdown_propagates_task_failure_and_retains_failure_on_repeat() {
    let (handle, http_port, proxy_port, _workspace) = start_test_http_server().await;
    let observer = ProxyObserver::new(&handle).await;
    handle
        .join_set
        .lock()
        .await
        .spawn(async { anyhow::bail!("injected HTTP task failure") });
    let result = handle.stop().await;
    let first_exit = ExitAtReturn::capture(&handle, &observer, http_port, proxy_port);
    let repeated = handle.clone().stop().await;
    let repeated_exit = ExitAtReturn::capture(&handle, &observer, http_port, proxy_port);
    observer.cleanup().await;

    assert!(
        first_exit.all_exited(),
        "first failed stop return: {first_exit:?}"
    );
    assert!(
        repeated_exit.all_exited(),
        "repeat failed stop return: {repeated_exit:?}"
    );
    let error = result.expect_err("HTTP task failure must propagate");
    assert!(error.to_string().contains("injected HTTP task failure"));
    assert_eq!(
        repeated
            .expect_err("repeated failure must persist")
            .to_string(),
        error.to_string()
    );
}
