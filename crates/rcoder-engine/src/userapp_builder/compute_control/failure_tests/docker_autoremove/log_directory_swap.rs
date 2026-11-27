//! 重启时运行时把可回收日志根下的日志挂载换成新目录，记在替代容器上：引擎按这份记录放行，
//! 新挂载写进完成记录；工作区换了（运行时报不报都一样）、记录的是别的容器、或者运行时没记
//! 的换源，一律拒绝。

use super::*;

const IDENTITY_CHANGED: &str = "Builder workspace volume identity changed during restart";

/// 原容器在可回收日志根下有一条日志挂载：重启时运行时把它换成新目录，并点名给引擎。引擎
/// 只放行这一条换源，重启照常收敛，新见证写进完成记录。
#[tokio::test]
async fn docker_autoremove_restart_allows_the_log_directory_the_runtime_replaces() {
    let root = tempfile::tempdir().unwrap();
    let (state, runtime, mode, pending, _directory) = fixture_with(Swaps {
        log: Some((
            root.path().join("logs/runner-old"),
            root.path().join("logs/runner-new"),
        )),
        declared: true,
        workspace_swap: false,
        ..Default::default()
    })
    .await;
    let result = execute_pending(&state, pending.clone()).await;
    assert!(
        result.is_ok(),
        "a restart that replaces a named log directory must converge: {result:?}"
    );
    let completed = read(&state, &pending).await;
    assert_completed(&runtime, &mode, &pending, &completed);
    // 恢复之前（Stopped）的见证记的是旧目录，之后记的是新目录
    assert_eq!(
        mode.restore_snapshots.lock().unwrap()[0].checkpoint["builder_volumes"][1]["uid"],
        serde_json::json!(root.path().join("logs/runner-old"))
    );
    assert_eq!(
        completed.checkpoint["builder_volumes"][1]["uid"],
        serde_json::json!(root.path().join("logs/runner-new"))
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

/// 续做路径同一条规则：恢复之后的卷校验放行被点名的日志挂载换源。
#[tokio::test]
async fn docker_autoremove_resume_allows_the_log_directory_the_runtime_replaces() {
    let root = tempfile::tempdir().unwrap();
    let (state, runtime, mode, pending, _directory) = fixture_with(Swaps {
        log: Some((
            root.path().join("logs/runner-old"),
            root.path().join("logs/runner-new"),
        )),
        declared: true,
        workspace_swap: false,
        ..Default::default()
    })
    .await;
    runtime.fail_stopped_capture.store(true, Ordering::SeqCst);
    assert!(execute_pending(&state, pending.clone()).await.is_err());
    let failure = read(&state, &pending).await;
    assert_failure(&failure, &pending, "stopped", STOPPED_FAILURE);
    runtime.fail_stopped_capture.store(false, Ordering::SeqCst);
    let resumed = resume_restart_start(&state, &failure).await;
    assert!(
        resumed.is_ok(),
        "a resumed restart must accept the replaced log directory too: {resumed:?}"
    );
    let completed = read(&state, &pending).await;
    assert_completed(&runtime, &mode, &pending, &completed);
    state.userapp_store_control.shutdown().await.unwrap();
}

/// 日志挂载换了源、工作区的 bind 源也变了：运行时只点名了日志那一条，工作区必须被拒绝。
#[tokio::test]
async fn docker_autoremove_restart_refuses_a_changed_workspace_volume() {
    let root = tempfile::tempdir().unwrap();
    let (state, runtime, _mode, pending, _directory) = fixture_with(Swaps {
        log: Some((
            root.path().join("logs/runner-old"),
            root.path().join("logs/runner-new"),
        )),
        declared: true,
        workspace_swap: true,
        ..Default::default()
    })
    .await;
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();
    assert!(format!("{error:#}").contains(IDENTITY_CHANGED), "{error:#}");
    let failure = read(&state, &pending).await;
    assert!(
        failure
            .error_message
            .as_deref()
            .unwrap()
            .contains("changed during restart"),
        "{failure:?}"
    );
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    state.userapp_store_control.shutdown().await.unwrap();
}

/// bind 源变了、运行时却没有点名这条挂载：哪怕变的是日志挂载，也按卷身份变了拒绝。
#[tokio::test]
async fn docker_autoremove_restart_refuses_a_swap_the_runtime_did_not_report() {
    let root = tempfile::tempdir().unwrap();
    let (state, runtime, _mode, pending, _directory) = fixture_with(Swaps {
        log: Some((
            root.path().join("logs/runner-old"),
            root.path().join("logs/runner-new"),
        )),
        declared: false,
        workspace_swap: false,
        ..Default::default()
    })
    .await;
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();
    assert!(format!("{error:#}").contains(IDENTITY_CHANGED), "{error:#}");
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    state.userapp_store_control.shutdown().await.unwrap();
}

/// 归档里的工作区碰巧落在日志根下面（旧容器、改过日志根的部署）：运行时不换它，重启前后是
/// 同一个目录。引擎按原源核对，重启照常收敛。
#[tokio::test]
async fn docker_autoremove_restart_keeps_a_workspace_that_lies_under_the_log_root() {
    let root = tempfile::tempdir().unwrap();
    let (state, runtime, mode, pending, _directory) = fixture_with(Swaps {
        log: Some((
            root.path().join("logs/runner-old"),
            root.path().join("logs/runner-new"),
        )),
        declared: true,
        workspace_swap: false,
        workspace_under_log_root: true,
        ..Default::default()
    })
    .await;
    let workspace = root.path().join("logs/runner-workspace");
    assert!(std::fs::read_to_string(workspace.join("sentinel")).is_ok());

    let result = execute_pending(&state, pending.clone()).await;

    assert!(result.is_ok(), "{result:?}");
    let completed = read(&state, &pending).await;
    assert_completed(&runtime, &mode, &pending, &completed);
    // Stopped 时的见证和完成记录里的，工作区都是原来那个目录；只有日志目录换了
    assert_eq!(
        mode.restore_snapshots.lock().unwrap()[0].checkpoint["builder_volumes"][0]["uid"],
        serde_json::json!(workspace)
    );
    assert_eq!(
        completed.checkpoint["builder_volumes"][0]["uid"],
        serde_json::json!(workspace)
    );
    assert_eq!(
        completed.checkpoint["builder_volumes"][1]["uid"],
        serde_json::json!(root.path().join("logs/runner-new"))
    );
    assert!(
        std::fs::read_to_string(workspace.join("sentinel")).is_ok(),
        "the workspace keeps its data"
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

/// 工作区的 bind 源变了，运行时还把它写进了换源记录：`/home/user` 下面的挂载是持久数据，
/// 谁报都不换。
#[tokio::test]
async fn docker_autoremove_restart_refuses_a_workspace_swap_however_it_is_reported() {
    let root = tempfile::tempdir().unwrap();
    let (state, runtime, _mode, pending, _directory) = fixture_with(Swaps {
        log: Some((
            root.path().join("logs/runner-old"),
            root.path().join("logs/runner-new"),
        )),
        declared: true,
        workspace_swap: true,
        workspace_declared: true,
        ..Default::default()
    })
    .await;
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();

    assert!(
        format!("{error:#}").contains(
            "Builder restart recorded an unexpected mount swap for /home/user/failureapp"
        ),
        "{error:#}"
    );
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    state.userapp_store_control.shutdown().await.unwrap();
}

/// 换源记录记的是另一个容器：重新采集到的挂载和它对不上号，记录不作数。
#[tokio::test]
async fn docker_autoremove_restart_refuses_a_witness_for_another_container() {
    let root = tempfile::tempdir().unwrap();
    let (state, runtime, _mode, pending, _directory) = fixture_with(Swaps {
        log: Some((
            root.path().join("logs/runner-old"),
            root.path().join("logs/runner-new"),
        )),
        declared: true,
        other_container: true,
        ..Default::default()
    })
    .await;
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();

    assert!(
        format!("{error:#}")
            .contains("Restart target was replaced before its mounts could be verified"),
        "{error:#}"
    );
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    state.userapp_store_control.shutdown().await.unwrap();
}

/// daemon 明确拒绝创建替代容器：重启停在可恢复的状态，报的是"被拒绝"而不是"结果不明"，
/// 也不启动、不放租约。
#[tokio::test]
async fn docker_autoremove_explicit_rejection_keeps_the_restart_recoverable() {
    let (state, runtime, mode, pending, _directory) = fixture().await;
    *mode.reject_with.lock().unwrap() = Some(400);
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("daemon refused the replacement"),
        "{error:#}"
    );
    let failure = read(&state, &pending).await;
    assert_failure(
        &failure,
        &pending,
        "stopped",
        "daemon refused the replacement",
    );
    // 直接观察到的拒绝是确定的结果，不按"结果不明"上报
    assert_eq!(failure.error_code.as_deref(), Some("ERR_BACKEND_ERROR"));
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);
    state.userapp_store_control.shutdown().await.unwrap();
}
