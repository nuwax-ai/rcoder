//! 创建方登记时的等待：按目录身份判断"要绑的是不是正被删的那个"，不比路径拼写。
//!
//! 判定和清理器那边的核对是同一个函数，只是角色对调：被删的目录当候选，要登记的路径当挂载源。

use super::*;
use crate::log_directory::{
    DirectoryBeingReclaimed, mark_reclaim, pending_bind_sources, reclaim_marks,
};
use crate::runtime::docker_runtime::host_path_space::HostPathSpace;

/// 给 `tree` 里的 `relative` 打上删除标记，宿主机写法是 `host`。标记随返回值 drop 解除
fn mark(tree: &Tree, relative: &str, host: &str) -> impl Drop {
    let (mark, view) = mark_reclaim(Path::new(host), &tree.path(relative)).unwrap();
    assert!(
        view.sources.is_empty(),
        "the table starts out with no registration in this test"
    );
    mark
}

/// 把等待时限缩到 20 毫秒：该等的会超时报错，不该等的照常通过
fn shorten_the_wait() {
    super::super::reclaim_wait::set_wait_limit_for_test(20);
}

async fn claim(tree: &Tree, host: &str) -> Result<(), DirectoryBeingReclaimed> {
    let resolver = tree.resolver();
    let space = HostPathSpace::Container(&resolver);
    PendingBinds::default()
        .claim(Path::new(host), Some(&space))
        .await
}

/// 拿到许可之后再登记同一个路径：创建方要等这次删除结束，不会绑到一个正在被删的目录上
#[tokio::test]
async fn a_claim_on_the_marked_directory_waits_for_the_deletion() {
    let tree = Tree::new();
    let held = mark(&tree, "logs/runner-a", REAL_A);
    let resolver = tree.resolver();
    let waiting = tokio::spawn(async move {
        let space = HostPathSpace::Container(&resolver);
        PendingBinds::default()
            .claim(Path::new(REAL_A), Some(&space))
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(
        reclaim_marks().len(),
        1,
        "the deletion is still holding its permit"
    );

    drop(held);
    assert!(
        waiting.await.unwrap().is_ok(),
        "the creator proceeds once the deletion is done"
    );
}

/// 回归（Codex 第五轮 P1）：清理器拿着 `logs/runner-a` 的许可，同一个目录经别名 `alias/runner-a`
/// 登记进来。只比路径拼写的话两者不相干，创建方直接放行，绑到的却是正在被删的那个目录。
#[tokio::test]
async fn a_claim_through_an_alias_waits_for_the_same_deletion() {
    super::super::reclaim_wait::set_wait_limit_for_test(2_000);
    let tree = Tree::new();
    let held = mark(&tree, "logs/runner-a", REAL_A);
    let resolver = tree.resolver();
    let waiting = tokio::spawn(async move {
        let space = HostPathSpace::Container(&resolver);
        PendingBinds::default()
            .claim(Path::new(ALIAS_A), Some(&space))
            .await
    });
    // 登记已经生效，但创建方在等：标记还在
    tokio::task::yield_now().await;
    assert!(pending_bind_sources().contains(&PathBuf::from(ALIAS_A)));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !waiting.is_finished(),
        "the creator must wait while the same directory is being deleted through another name"
    );

    drop(held);
    assert!(waiting.await.unwrap().is_ok());
}

/// 删除一直不结束：经别名登记的创建方同样等不过时限，报的是被删的那个目录
#[tokio::test]
async fn a_claim_through_an_alias_gives_up_when_the_deletion_never_ends() {
    shorten_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    let waited = claim(&tree, ALIAS_A).await;

    assert_eq!(
        waited.err(),
        Some(DirectoryBeingReclaimed(PathBuf::from(REAL_A)))
    );
}

/// 反过来，别名在清理器那头：标记的是 `alias/runner-a`，创建方按真实路径登记，一样要等
#[tokio::test]
async fn a_marked_alias_holds_a_claim_by_its_real_name() {
    shorten_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "alias/runner-a", ALIAS_A);

    assert!(claim(&tree, REAL_A).await.is_err());
}

/// 标记盖住整个子树：登记它里面的路径也要等
#[tokio::test]
async fn a_claim_below_the_marked_directory_waits() {
    shorten_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    assert!(
        claim(&tree, "/host/base/logs/runner-a/log").await.is_err(),
        "a bind inside the deleted directory waits for the same deletion"
    );
}

/// 别的目录不受影响：同一个标记在，登记兄弟目录不用等
#[tokio::test]
async fn a_claim_on_another_directory_does_not_wait() {
    shorten_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    assert!(
        claim(&tree, "/host/base/logs/runner-b").await.is_ok(),
        "waiting would have timed out"
    );
}

/// 登记的是被删目录的**上级**：不用等。挂着一个上级目录，不会让它下面每个过期目录都算作仍被
/// 占用——和清理器的清单同一个口径
#[tokio::test]
async fn a_claim_on_the_parent_of_the_marked_directory_does_not_wait() {
    shorten_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    assert!(claim(&tree, "/host/base/logs").await.is_ok());
}

/// 要绑的目录定不了位（它经过一个指到 RCoder 挂载之外的链接）：证明不了它和正在删的目录不相干，
/// 宁可等它删完
#[tokio::test]
async fn a_claim_whose_location_cannot_be_confirmed_waits() {
    shorten_the_wait();
    let tree = Tree::new();
    let unseen = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(unseen.path(), tree.path("logs/elsewhere")).unwrap();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    let waited = claim(&tree, "/host/base/logs/elsewhere/log").await;

    assert!(
        waited.is_err(),
        "cannot prove it is unrelated, so it waits: {waited:?}"
    );
}

/// 没有路径空间就核不了身份：有目录在删就宁可等它删完，哪怕登记的是别的目录
#[tokio::test]
async fn without_a_path_space_a_claim_waits_for_every_deletion() {
    shorten_the_wait();
    let tree = Tree::new();
    let held = mark(&tree, "logs/runner-a", REAL_A);

    let waited = PendingBinds::default()
        .claim(Path::new("/host/base/logs/runner-b"), None)
        .await;
    assert!(waited.is_err(), "identity cannot be checked, so it waits");

    drop(held);
    assert!(
        PendingBinds::default()
            .claim(Path::new("/host/base/logs/runner-b"), None)
            .await
            .is_ok()
    );
}
