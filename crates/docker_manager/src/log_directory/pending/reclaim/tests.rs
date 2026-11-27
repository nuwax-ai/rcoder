//! 删除和登记的目录级互斥：清理器先标记再核对，创建方先登记再等。
//!
//! 核对（"有没有创建方登记着它"）不在这一层：它用的判定和清理器取清单时一致，在
//! `runtime::docker_runtime::log_dir_mounts::begin_reclaim`。这里只有标记和等待。

use super::*;
use crate::log_directory::{PendingBinds, pending_bind_sources};

/// 同一个目录只许一个清理器在删：第二个标记被拒绝，这一轮不再碰它
#[test]
fn a_directory_cannot_be_marked_twice() {
    let first = mark_reclaim(Path::new("/srv/logs/container/runner-a")).unwrap();

    assert!(
        mark_reclaim(Path::new("/srv/logs/container/runner-a")).is_err(),
        "another sweep is already deleting this directory"
    );

    drop(first);
    assert!(mark_reclaim(Path::new("/srv/logs/container/runner-a")).is_ok());
}

/// 标记按归一化之后的路径比：同一个目录的两种写法只算一个
#[test]
fn a_mark_is_keyed_by_its_normalized_path() {
    let first = mark_reclaim(Path::new("/srv/logs/container/./runner-a")).unwrap();

    assert!(mark_reclaim(Path::new("/srv/logs/container/runner-a")).is_err());

    drop(first);
}

/// 登记一个正被删的目录：等这次删除结束。等到了目录已经不在，创建由 Docker 明确拒绝，不会绑到
/// 一个正在被删的目录上
#[tokio::test]
async fn a_creator_waits_for_the_deletion_of_the_directory_it_reuses() {
    let mark = mark_reclaim(Path::new("/srv/logs/container/runner-a")).unwrap();
    let waiting = tokio::spawn(async {
        let mut binds = PendingBinds::default();
        binds
            .claim(Path::new("/srv/logs/container/runner-a"))
            .await
    });
    // 登记在等待之前就生效：清理器读得到它
    tokio::task::yield_now().await;
    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from("/srv/logs/container/runner-a")],
        "the claim is registered before the creator starts waiting"
    );

    // 删了 30 毫秒：登记方等到它结束
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(mark);
    let claimed = waiting.await.unwrap();
    assert!(claimed.is_ok(), "the creator proceeds once the deletion is done");
}

/// 删除一直不结束：等不过 [`RECLAIM_WAIT_LIMIT`]，报错
#[tokio::test]
async fn a_creator_gives_up_waiting_for_a_deletion_that_never_ends() {
    RECLAIM_WAIT_LIMIT_MILLIS.store(20, Ordering::SeqCst);
    let _mark = mark_reclaim(Path::new("/srv/logs/container/runner-a")).unwrap();

    let waited = PendingBinds::default()
        .claim(Path::new("/srv/logs/container/runner-a"))
        .await;

    assert_eq!(
        waited.err(),
        Some(DirectoryBeingReclaimed(PathBuf::from(
            "/srv/logs/container/runner-a"
        )))
    );
    assert!(
        DirectoryBeingReclaimed(PathBuf::from("/srv/logs/container/runner-a"))
            .to_string()
            .contains("still being reclaimed"),
        "the message must be a full English sentence"
    );
}

/// 标记的是别的目录：登记直接通过，不等待
#[tokio::test]
async fn a_claim_on_a_directory_nobody_is_deleting_returns_at_once() {
    let mark = mark_reclaim(Path::new("/srv/logs/container/runner-b")).unwrap();

    let claimed = PendingBinds::default()
        .claim(Path::new("/srv/logs/container/runner-a"))
        .await;

    assert!(claimed.is_ok());
    drop(mark);
}

/// 等的是**被删目录里面**的路径：标记覆盖整个子树
#[tokio::test]
async fn a_creator_waits_for_the_deletion_of_the_directory_holding_its_bind() {
    RECLAIM_WAIT_LIMIT_MILLIS.store(200, Ordering::SeqCst);
    let mark = mark_reclaim(Path::new("/srv/logs/container/runner-a")).unwrap();
    let waiting = tokio::spawn(async {
        let mut binds = PendingBinds::default();
        binds
            .claim(Path::new("/srv/logs/container/runner-a/log"))
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(
        pending_bind_sources(),
        [PathBuf::from("/srv/logs/container/runner-a/log")]
    );

    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(mark);
    assert!(
        waiting.await.unwrap().is_ok(),
        "a bind inside the deleted directory waits for the same deletion"
    );
}

/// 标记解除之后再登记同一个路径，不再等待
#[tokio::test]
async fn a_claim_after_the_mark_is_gone_does_not_wait() {
    let mark = mark_reclaim(Path::new("/srv/logs/container/runner-a")).unwrap();
    drop(mark);

    let claimed = PendingBinds::default()
        .claim(Path::new("/srv/logs/container/runner-a"))
        .await;

    assert!(claimed.is_ok());
}

/// 新建的日志目录不会撞上删除：它的名字是新的，不在任何标记里
#[test]
fn a_fresh_directory_is_never_under_a_reclaim_mark() {
    let mark = mark_reclaim(Path::new("/srv/logs/container/runner-a-20261029035324")).unwrap();

    let fresh = Path::new("/srv/logs/container/runner-a-20261127060000");
    assert!(
        !being_reclaimed(&lock(), fresh),
        "a fresh directory has its own name and is never inside a marked one"
    );
    drop(mark);
}
