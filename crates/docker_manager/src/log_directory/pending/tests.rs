//! 占用登记的测试：在途登记不淘汰、守卫 drop 时注销；请求发出后结果不明的转去带过期时间的
//! 表；登记跟着创建请求的结果走。

use super::*;

fn normalize(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths
}

#[test]
fn a_claim_is_visible_until_its_guard_is_dropped() {
    assert!(pending_bind_sources().is_empty());

    let mut binds = PendingBinds::default();
    binds.claim(Path::new("/srv/logs/container/runner-a"));
    binds.claim(Path::new("/srv/logs/container/runner-b"));
    assert_eq!(
        normalize(pending_bind_sources()),
        [
            PathBuf::from("/srv/logs/container/runner-a"),
            PathBuf::from("/srv/logs/container/runner-b")
        ]
    );

    drop(binds);
    assert!(pending_bind_sources().is_empty());
}

#[test]
fn a_claim_is_keyed_by_its_normalized_path() {
    let mut binds = PendingBinds::default();
    binds.claim(Path::new("/srv/logs/container/./runner-a/"));

    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from("/srv/logs/container/runner-a")]
    );
}

/// 注销只动自己登记过的路径
#[test]
fn dropping_one_guard_leaves_the_others_claimed() {
    let mut first = PendingBinds::default();
    first.claim(Path::new("/srv/logs/container/runner-a"));
    let mut second = PendingBinds::default();
    second.claim(Path::new("/srv/logs/container/runner-b"));

    drop(first);

    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from("/srv/logs/container/runner-b")]
    );
}

/// 在途登记不淘汰：再多活守卫，登记也一条不少
#[test]
fn live_claims_are_never_evicted() {
    let guards: Vec<PendingBinds> = (0..20_000)
        .map(|index| {
            let mut binds = PendingBinds::default();
            binds.claim(&PathBuf::from(format!(
                "/srv/logs/container/runner-{index}"
            )));
            binds
        })
        .collect();

    assert_eq!(pending_bind_sources().len(), guards.len());

    drop(guards);
    assert!(pending_bind_sources().is_empty());
}

/// 请求发出后守卫 drop 了却没人交代结果：按结果不明处理，登记留着
#[test]
fn a_guard_dropped_after_the_request_was_sent_keeps_its_claims() {
    let mut binds = PendingBinds::default();
    binds.claim(Path::new("/srv/logs/container/runner-a"));
    binds.request_sent();

    drop(binds);

    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from("/srv/logs/container/runner-a")]
    );
}

/// 请求发出之前就 drop（创建流程失败、被取消）：登记注销
#[test]
fn a_guard_dropped_before_the_request_was_sent_releases_its_claims() {
    let mut binds = PendingBinds::default();
    binds.claim(Path::new("/srv/logs/container/runner-a"));

    drop(binds);

    assert!(pending_bind_sources().is_empty());
}

/// 结果不明的登记到时间会消失：没人用的目录不该被永久保护
#[tokio::test]
async fn an_unknown_outcome_expires() {
    assert!(UNRESOLVED.get().is_none(), "the table must not exist yet");
    // 比默认的兜底时长短得多
    UNRESOLVED_TTL_MILLIS.store(10, Ordering::SeqCst);
    let mut binds = PendingBinds::default();
    binds.claim(Path::new("/srv/logs/container/runner-a"));
    binds.request_sent();

    drop(binds);
    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from("/srv/logs/container/runner-a")]
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        pending_bind_sources().is_empty(),
        "the unresolved claim must expire"
    );
}

#[tokio::test]
async fn a_request_that_succeeds_releases_the_claims() {
    let mut binds = PendingBinds::default();
    binds.claim(Path::new("/srv/logs/container/runner-a"));

    let created = binds.until_settled(async { Ok("container-id") }).await;

    assert_eq!(created.unwrap(), "container-id");
    drop(binds);
    assert!(pending_bind_sources().is_empty());
}

/// 请求报错分不清是被拒绝了还是应答丢了：按结果不明处理，登记留着
#[tokio::test]
async fn a_request_that_fails_keeps_the_claims() {
    let mut binds = PendingBinds::default();
    binds.claim(Path::new("/srv/logs/container/runner-a"));

    let failed = binds
        .until_settled(async {
            Err::<(), _>(crate::DockerError::ConnectionError(
                "connection reset".to_owned(),
            ))
        })
        .await;

    assert!(failed.is_err());
    drop(binds);
    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from("/srv/logs/container/runner-a")]
    );
}

/// 等应答的时候这个 future 被取消（任务被 abort）：守卫在 future 里跟着 drop，登记留着
#[tokio::test]
async fn a_request_cancelled_while_waiting_keeps_the_claims() {
    let (waiting, is_waiting) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async {
        let mut binds = PendingBinds::default();
        binds.claim(Path::new("/srv/logs/container/runner-a"));
        let request = binds.until_settled(std::future::pending::<crate::DockerResult<()>>());
        let _ = waiting.send(());
        request.await
    });
    is_waiting.await.unwrap();

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from("/srv/logs/container/runner-a")]
    );
}

/// future 还没被轮询过就被丢弃：请求根本没发出去，登记注销
#[tokio::test(flavor = "current_thread")]
async fn a_request_dropped_before_it_was_polled_releases_the_claims() {
    let task = tokio::spawn(async {
        let mut binds = PendingBinds::default();
        binds.claim(Path::new("/srv/logs/container/runner-a"));
        binds
            .until_settled(std::future::pending::<crate::DockerResult<()>>())
            .await
    });

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    assert!(pending_bind_sources().is_empty());
}

#[test]
fn an_unknown_outcome_keeps_the_claims_and_the_guard_can_then_be_dropped() {
    let mut binds = PendingBinds::default();
    binds.claim(Path::new("/srv/logs/container/runner-a"));
    binds.request_sent();
    binds.outcome_unknown();

    drop(binds);

    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from("/srv/logs/container/runner-a")]
    );
}
