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

/// 把等待时限缩到 1 秒（生产是 10 秒）：该等的最多等 1 秒就报错，不该等的探测完就通过。
///
/// 时限管的是探测加等待。缩得太短（试过 20 毫秒）的话，机器负载高时真实的文件系统探测跑不完，
/// 不该等的也会报错；更糟的是该等的用例会因为"探测超时"假通过——冲突判定坏了它们照样绿。
/// 1 秒足够探测在任何负载下跑完
fn bound_the_wait() {
    super::super::reclaim_wait::set_wait_limit_for_test(1_000);
}

async fn claim(tree: &Tree, host: &str) -> Result<(), DirectoryBeingReclaimed> {
    let resolver = tree.resolver();
    let space = HostPathSpace::Container(&resolver);
    PendingBinds::default()
        .claim(Path::new(host), Some(&space))
        .await
}

/// 登记等到了时限，而且是因为**确认了冲突**（不是探测没跑完），报的是 `expected` 这个目录
fn assert_waited_out(waited: Result<(), DirectoryBeingReclaimed>, expected: &str, why: &str) {
    let error = waited.expect_err(why);
    assert!(error.is_conflict(), "{why}; instead: {error}");
    assert_eq!(error.directory(), Path::new(expected), "{why}");
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
    // 给足探测的时间：冲突判定要是坏了，登记早就通过了
    tokio::time::sleep(Duration::from_millis(300)).await;
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
    bound_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    let waited = claim(&tree, ALIAS_A).await;

    assert_waited_out(
        waited,
        REAL_A,
        "the same directory registered through an alias",
    );
}

/// 反过来，别名在清理器那头：标记的是 `alias/runner-a`，创建方按真实路径登记，一样要等
#[tokio::test]
async fn a_marked_alias_holds_a_claim_by_its_real_name() {
    bound_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "alias/runner-a", ALIAS_A);

    assert_waited_out(
        claim(&tree, REAL_A).await,
        ALIAS_A,
        "marked through the alias",
    );
}

/// 标记盖住整个子树：登记它里面的路径也要等
#[tokio::test]
async fn a_claim_below_the_marked_directory_waits() {
    bound_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    assert_waited_out(
        claim(&tree, "/host/base/logs/runner-a/log").await,
        REAL_A,
        "a bind inside the deleted directory waits for the same deletion",
    );
}

/// 别的目录不受影响：同一个标记在，登记兄弟目录不用等
#[tokio::test]
async fn a_claim_on_another_directory_does_not_wait() {
    bound_the_wait();
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
    bound_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    assert!(claim(&tree, "/host/base/logs").await.is_ok());
}

/// 要绑的目录定不了位（它经过一个指到 RCoder 挂载之外的链接）：证明不了它和正在删的目录不相干，
/// 宁可等它删完
#[tokio::test]
async fn a_claim_whose_location_cannot_be_confirmed_waits() {
    bound_the_wait();
    let tree = Tree::new();
    let unseen = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(unseen.path(), tree.path("logs/elsewhere")).unwrap();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    let waited = claim(&tree, "/host/base/logs/elsewhere/log").await;

    assert_waited_out(waited, REAL_A, "cannot prove it is unrelated, so it waits");
}

/// 回归（Codex 第六轮 P1）：要绑的路径还没建出来，经别名落在一个正在删的目录里面。路径不存在，
/// 没有身份可比——看它会建在哪一级已存在的祖先下面：祖先是正在删的那个目录。
#[tokio::test]
async fn a_path_not_created_yet_under_an_alias_waits() {
    bound_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    let waited = claim(&tree, "/host/base/alias/runner-a/not-created").await;

    assert_waited_out(
        waited,
        REAL_A,
        "the path will be created inside the directory being deleted",
    );
}

/// 删完之后登记就通过：那时目录已经不在，创建方自己把它建出来
#[tokio::test]
async fn a_path_not_created_yet_proceeds_once_the_deletion_is_done() {
    bound_the_wait();
    let tree = Tree::new();
    let held = mark(&tree, "logs/runner-a", REAL_A);
    assert_waited_out(
        claim(&tree, "/host/base/alias/runner-a/not-created").await,
        REAL_A,
        "it waits while the deletion is in progress",
    );

    drop(held);

    assert!(
        claim(&tree, "/host/base/alias/runner-a/not-created")
            .await
            .is_ok()
    );
}

/// 反过来：标记的是别名 `alias/runner-a`，还没建出来的路径按真实名字登记。两边都按真实位置比
#[tokio::test]
async fn a_path_not_created_yet_under_the_real_name_waits_for_a_marked_alias() {
    bound_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "alias/runner-a", ALIAS_A);

    let waited = claim(&tree, "/host/base/logs/runner-a/not-created").await;

    assert_waited_out(
        waited,
        ALIAS_A,
        "the path will be created inside the directory being deleted",
    );
}

/// 已经在了的路径和被删目录不相干，就不等——哪怕被删目录自己的真实位置定不下来。祖先比对只
/// 为"还没建出来"的路径做：已经在了的路径按身份判完就完，不因为被删目录定不了位而陪着等
#[tokio::test]
async fn an_existing_unrelated_directory_does_not_wait_even_if_the_marked_one_cannot_be_located() {
    bound_the_wait();
    let tree = Tree::new();
    // 被删的目录经过一个指到 RCoder 挂载之外的链接：它的真实位置定不下来
    let unseen = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(unseen.path(), tree.path("logs/elsewhere")).unwrap();
    std::fs::create_dir_all(unseen.path().join("runner-a")).unwrap();
    std::fs::create_dir_all(tree.path("logs/runner-b")).unwrap();
    let _held = mark(
        &tree,
        "logs/elsewhere/runner-a",
        "/host/base/logs/elsewhere/runner-a",
    );

    assert!(
        claim(&tree, "/host/base/logs/runner-b").await.is_ok(),
        "an existing unrelated directory must not wait; waiting would have timed out"
    );
}

/// 还没建出来的路径落在被删目录的**旁边**（兄弟目录下面）或者**上面**（日志根下面）：不等。
/// 等的话会超时报错，所以 `is_ok` 就证明没等
#[tokio::test]
async fn a_path_not_created_yet_outside_the_marked_directory_does_not_wait() {
    bound_the_wait();
    let tree = Tree::new();
    let _held = mark(&tree, "logs/runner-a", REAL_A);

    assert!(
        claim(&tree, "/host/base/alias/runner-new/not-created")
            .await
            .is_ok()
    );
    assert!(claim(&tree, "/host/base/logs/not-created").await.is_ok());
}

/// 回归（Codex 第六轮 P2）：目录身份探测自己卡住（文件系统不响应）。截止时间管的是整个调用——
/// 探测和等待加在一起不能超过时限，而不是等探测返回才开始算。
#[test]
fn the_wait_limit_covers_the_identity_probe_too() {
    // 只有一个阻塞线程的运行时，先把它占住：探测用的文件系统调用排不上队
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        super::super::reclaim_wait::set_wait_limit_for_test(20);
        let tree = Tree::new();
        let held = mark(&tree, "logs/runner-a", REAL_A);
        let occupant =
            tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(400)));

        let started = std::time::Instant::now();
        let waited = tokio::time::timeout(
            Duration::from_millis(200),
            claim(&tree, "/host/base/alias/runner-a/not-created"),
        )
        .await;

        assert!(
            waited.is_ok(),
            "the inner limit must fire long before the outer guard"
        );
        // 报的是"判不出来"，不是"确认了冲突"：排查的人不该去找一个并不存在的删除。点名的仍是
        // 正在删的那个目录
        let error = waited.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot confirm within the wait limit"),
            "{error}"
        );
        assert!(!error.is_conflict(), "{error}");
        assert_eq!(error.directory(), Path::new(REAL_A));
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "the call returned at the inner limit, not at the outer guard"
        );
        occupant.await.unwrap();
        drop(held);
    });
}

/// 没有路径空间就核不了身份：有目录在删就宁可等它删完，哪怕登记的是别的目录
#[tokio::test]
async fn without_a_path_space_a_claim_waits_for_every_deletion() {
    bound_the_wait();
    let tree = Tree::new();
    let held = mark(&tree, "logs/runner-a", REAL_A);

    let waited = PendingBinds::default()
        .claim(Path::new("/host/base/logs/runner-b"), None)
        .await;
    assert_waited_out(waited, REAL_A, "identity cannot be checked, so it waits");

    drop(held);
    assert!(
        PendingBinds::default()
            .claim(Path::new("/host/base/logs/runner-b"), None)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn missing_child_of_the_same_physical_directory_via_another_mount_waits() {
    bound_the_wait();
    let tree = Tree::new();
    let direct = tree.path("logs");
    let mirrored_parent = tree.path("mirrored-parent");
    std::os::unix::fs::symlink(&tree.base, &mirrored_parent).unwrap();
    let mirror = mirrored_parent.join("logs");
    assert_eq!(
        direct.canonicalize().unwrap(),
        mirror.canonicalize().unwrap()
    );
    let resolver = HostPathResolver::from_mount_table(vec![
        (direct.clone(), PathBuf::from("/host/actual")),
        (mirror, PathBuf::from("/host/another-mount")),
    ])
    .unwrap();
    let space = HostPathSpace::Container(&resolver);
    let deleted = "/host/actual/runner-a";
    let _held = mark_reclaim(Path::new(deleted), &direct.join("runner-a")).unwrap();
    let mut binds = PendingBinds::default();
    let result = binds
        .claim(
            Path::new("/host/another-mount/runner-a/new-dir"),
            Some(&space),
        )
        .await;
    assert_waited_out(
        result,
        deleted,
        "both views of the existing ancestor have the same filesystem identity",
    );
}
