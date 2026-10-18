use super::*;
use process_wrap::tokio::{CommandWrap, ProcessGroup};
use std::process::Stdio;
use std::time::Duration;

async fn held_process() -> (AgentLifecycleGuard, tempfile::TempDir, u32) {
    held_process_with_session(SessionId::from("held-session")).await
}

async fn held_process_with_session(
    session_id: SessionId,
) -> (AgentLifecycleGuard, tempfile::TempDir, u32) {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut command = CommandWrap::with_new("sh", |command| {
        command
            .args([
                "-c",
                "trap '' TERM; touch \"$1\"; while :; do sleep 60; done",
                "fixture",
            ])
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    });
    let child = command.wrap(ProcessGroup::leader()).spawn().unwrap();
    let pid = child.id().unwrap();
    let guard = AgentLifecycleGuard::new_claude(
        "held".into(),
        session_id,
        child,
        tokio::spawn(async {}),
        CancellationToken::new(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (guard, directory, pid)
}

#[tokio::test]
async fn unassigned_process_has_no_session_identity_and_only_accepts_a_real_handshake() {
    let (mut guard, _directory, _pid) =
        held_process_with_session(SessionId::new(Arc::from(""))).await;
    assert!(guard.inner.session_id.is_none());
    for sid in ["", "pending"] {
        assert!(guard.set_session_id(SessionId::from(sid)).is_err());
        assert!(guard.inner.session_id.is_none());
    }
    guard
        .set_session_id(SessionId::from("opaque/session:7"))
        .unwrap();
    assert_eq!(
        guard.inner.session_id.as_ref().map(|sid| sid.0.as_ref()),
        Some("opaque/session:7")
    );
    guard.graceful_stop().await.unwrap();
}

#[tokio::test]
async fn cancel_is_not_exit_and_all_stop_callers_wait_for_reaping() {
    let (guard, _directory, pid) = held_process().await;
    guard.cancel();
    assert!(!guard.is_stopped());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), guard.wait_for_exit())
            .await
            .is_err()
    );
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok());
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(guard.graceful_stop(), guard.graceful_stop())
    })
    .await
    .unwrap();
    first.unwrap();
    second.unwrap();
    assert!(guard.is_stopped());
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    );
}

#[tokio::test]
async fn interrupted_stop_observer_cannot_interrupt_process_cleanup() {
    let (guard, _directory, pid) = held_process().await;
    let stop_guard = guard.clone();
    let task = tokio::spawn(async move { stop_guard.graceful_stop().await });
    tokio::time::timeout(
        Duration::from_secs(2),
        guard.cancellation_token().cancelled(),
    )
    .await
    .unwrap();
    task.abort();
    tokio::time::timeout(Duration::from_secs(5), guard.wait_for_exit())
        .await
        .unwrap()
        .unwrap();
    assert!(guard.is_stopped());
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    );
}

#[tokio::test]
async fn cancelled_startup_retains_transition_until_actual_reaping() {
    let (guard, _directory, pid) = held_process().await;
    let project = format!("startup-barrier-{pid}");
    let transition = Arc::new(crate::session::lock_execution_transition(&project).await);
    let completed = guard.retain_startup_transition(transition.clone());
    drop(transition);
    drop(completed);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            crate::session::lock_execution_transition(&project)
        )
        .await
        .is_err(),
        "cancelled handshake must retain admission until child.wait"
    );
    drop(guard);
    let _next = tokio::time::timeout(
        Duration::from_secs(5),
        crate::session::lock_execution_transition(&project),
    )
    .await
    .unwrap();
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    );
}

#[tokio::test]
async fn successful_startup_releases_transition_without_stopping_agent() {
    let (guard, _directory, pid) = held_process().await;
    let project = format!("successful-startup-barrier-{pid}");
    let transition = Arc::new(crate::session::lock_execution_transition(&project).await);
    let completed = guard.retain_startup_transition(transition.clone());
    drop(transition);
    completed.send(()).unwrap();
    let _next = tokio::time::timeout(
        Duration::from_secs(5),
        crate::session::lock_execution_transition(&project),
    )
    .await
    .unwrap();
    assert!(!guard.is_stopped());
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok());
    guard.graceful_stop().await.unwrap();
}
