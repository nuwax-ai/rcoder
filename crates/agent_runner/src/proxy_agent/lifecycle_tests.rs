use super::*;
use std::time::Duration;

/// 用真实启动、监听和 join 确认 stop 不会把受理关闭当作退出完成。
#[tokio::test]
async fn stop_waits_for_real_proxy_exit_and_releases_listener() {
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve proxy port");
    let listen_port = reservation.local_addr().expect("proxy address").port();
    drop(reservation);
    let config = ProxyConfig {
        listen_port,
        ..Default::default()
    };
    let mut proxy = start_pingora(&config, Arc::new(DashMap::new())).expect("start proxy");
    let listening = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", listen_port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    let stop_result = proxy.stop().await;
    // current-thread runtime 保证旧的立即返回 stop 尚未让后台任务继续推进。
    let task_finished_at_return = proxy
        .server_task
        .as_ref()
        .is_none_or(tokio::task::JoinHandle::is_finished);
    let listener_released_at_return = TcpListener::bind(("0.0.0.0", listen_port)).is_ok();
    let health_finished_at_return = proxy
        .health_task
        .as_ref()
        .is_none_or(tokio::task::JoinHandle::is_finished);

    // 先实际 join 再断言，失败反例同样完整清理代理和健康检查。
    if let Some(task) = proxy.server_task.take() {
        tokio::time::timeout(Duration::from_secs(12), task)
            .await
            .expect("proxy cleanup deadline")
            .expect("join proxy task")
            .expect("proxy cleanup result");
    }
    proxy.health_stop.send_replace(true);
    if let Some(task) = proxy.health_task.take() {
        task.await.expect("join health check cleanup");
    }

    assert!(listening.is_ok(), "real Pingora proxy never listened");
    stop_result.expect("confirmed proxy shutdown");
    assert!(
        task_finished_at_return && listener_released_at_return && health_finished_at_return,
        "stop returned before real proxy exit: task_finished={task_finished_at_return}, listener_released={listener_released_at_return}, health_finished={health_finished_at_return}"
    );
    proxy.stop().await.expect("repeat confirmed shutdown");
}

/// 协议级故障注入补充：等待超时不丢所有权，不增加重试预算，也不取消健康任务。
#[tokio::test]
async fn unknown_proxy_exit_retains_tasks_deadline_and_failure_on_repeat_stop() {
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(async move {
        let _ = shutdown_rx.await;
        let _ = released.await;
        anyhow::bail!("injected native thread exit remains unconfirmed")
    });
    let (health_stop, mut health_stopped) = tokio::sync::watch::channel(false);
    let health_task = tokio::spawn(async move {
        while !*health_stopped.borrow_and_update() {
            if health_stopped.changed().await.is_err() {
                break;
            }
        }
    });
    let mut proxy = PingoraStartResult {
        shutdown_tx: Some(shutdown_tx),
        server_task: Some(server_task),
        health_stop,
        health_task: Some(health_task),
        shutdown_deadline: None,
        shutdown_error: None,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_millis(30);
    let timeout_result = proxy.stop_until(deadline).await;
    let task_retained = proxy
        .server_task
        .as_ref()
        .is_some_and(|task| !task.is_finished());
    let health_retained = !*proxy.health_stop.borrow()
        && proxy
            .health_task
            .as_ref()
            .is_some_and(|task| !task.is_finished());

    release.send(()).expect("release injected proxy completion");
    while proxy
        .server_task
        .as_ref()
        .is_some_and(|task| !task.is_finished())
    {
        tokio::task::yield_now().await;
    }
    let failed_result = proxy
        .stop_until(tokio::time::Instant::now() + Duration::from_secs(10))
        .await;
    let repeated_result = proxy.stop().await;
    let deadline_unchanged = proxy.shutdown_deadline == Some(deadline);
    let health_kept_after_failure = !*proxy.health_stop.borrow();

    // 故障用例独立清理；清理动作不能改变上面捕获的业务结果。
    proxy.health_stop.send_replace(true);
    if let Some(task) = proxy.health_task.take() {
        task.await.expect("join injected health task");
    }
    if let Some(task) = proxy.server_task.take() {
        drop(task.await.expect("join injected proxy task"));
    }

    assert!(
        timeout_result
            .expect_err("unknown exit must fail")
            .to_string()
            .contains("task exit remains unconfirmed")
    );
    assert!(task_retained && health_retained);
    assert!(deadline_unchanged && health_kept_after_failure);
    let failed_error = failed_result.expect_err("native exit failure must propagate");
    assert!(
        failed_error
            .to_string()
            .contains("injected native thread exit remains unconfirmed")
    );
    assert_eq!(
        repeated_result
            .expect_err("repeated stop must retain failure")
            .to_string(),
        format!("{failed_error:#}")
    );
}
