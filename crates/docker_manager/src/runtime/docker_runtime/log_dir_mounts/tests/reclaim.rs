//! 清理器删目录之前的最后一次确认：先标记、后核对；这一轮的视图（登记加 Docker 清单）记着
//! 取它时登记表的代次，代次变了就在标记之下重取，并且整轮换成新的。

use super::*;
use crate::log_directory::pending_bind_sources;
use std::sync::atomic::{AtomicBool, Ordering};

/// 假 daemon 收到的容器列表请求数
fn list_requests(daemon: &FakeDaemon) -> usize {
    daemon
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("/containers/json"))
        .count()
}

/// 登记表动一下（一次登记加上、再注销），视图的代次就对不上了
fn move_the_registry() {
    let mut binds = PendingBinds::default();
    binds.claim_blocking(Path::new("/host/base/logs/somewhere-else"));
    drop(binds);
}

/// 清单之后走完的一次创建：创建方登记 `host`，假 daemon 从此报告有容器挂着它，创建方注销。
/// 登记表里没有它，容器只在 Docker 里
async fn a_creation_finishing_after_the_inventory(mounted: &AtomicBool, host: &str) {
    let mut late = PendingBinds::default();
    late.claim(Path::new(host), None).await.unwrap();
    mounted.store(true, Ordering::SeqCst);
}

/// 回归（Codex 第四轮 P1）：创建方在清理器取完清单之后才登记了一个已过期的旧目录，容器还没建
/// 出来。清单里没有它，但最后一次确认读得到登记：许可被拒，目录保留。
#[tokio::test]
async fn a_directory_claimed_after_the_inventory_is_refused_the_reclaim() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.path("logs/runner-a")).unwrap();
    let daemon = fake_daemon(json!([])).await;
    let runtime = runtime(daemon.address, tree.resolver());
    let reused = tree.path("logs/runner-a");
    runtime
        .mounted_log_directories(std::slice::from_ref(&reused))
        .await
        .unwrap();
    let mut pending = PendingBinds::default();
    pending.claim(Path::new(REAL_A), None).await.unwrap();

    let refused = runtime.begin_log_directory_reclaim(&reused).await;

    assert!(
        matches!(refused, Err(ReclaimRefused::InUse(_))),
        "a directory a starting container registered must not be deleted: {refused:?}"
    );
    // 别的目录照样能删
    assert!(
        runtime
            .begin_log_directory_reclaim(&tree.path("logs/other"))
            .await
            .is_ok()
    );
}

/// 登记落在清理器读登记表**之后**、取 Docker 清单之前：登记先读、清单后取，它两头都没赶上，
/// 这一份视图里没有它。删除之前的最后一次确认在标记之后读登记和代次，看得到。
#[tokio::test]
async fn a_directory_registered_between_the_registry_read_and_the_docker_list_is_refused() {
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    static GUARD: Mutex<Option<PendingBinds>> = Mutex::new(None);
    let tree = Tree::new();
    std::fs::create_dir_all(tree.path("logs/runner-a")).unwrap();
    let daemon = fake_daemon_hooked(json!([]), || {
        if !REGISTERED.swap(true, Ordering::SeqCst) {
            let mut binds = PendingBinds::default();
            binds.claim_blocking(Path::new(REAL_A));
            *GUARD.lock().unwrap() = Some(binds);
        }
    })
    .await;
    let runtime = runtime(daemon.address, tree.resolver());
    let reused = tree.path("logs/runner-a");

    let inventory = runtime
        .mounted_log_directories(std::slice::from_ref(&reused))
        .await
        .unwrap();
    assert!(
        inventory.mounted.is_empty(),
        "the registration came after the registry was read for this view"
    );

    let refused = runtime.begin_log_directory_reclaim(&reused).await;

    assert!(
        matches!(refused, Err(ReclaimRefused::InUse(_))),
        "{refused:?}"
    );
    *GUARD.lock().unwrap() = None;
}

/// 回归（Codex 第五轮 P1）：创建在清理器取完清单**之后**走完了全程——创建方登记旧目录、Docker
/// 把容器建出来、创建方注销。登记表里已经没有它，那份清单里也没有它：只读登记表的最后一次
/// 确认看不到这个容器，会给一个正在被绑定的目录发删除许可。
///
/// 每份视图记着取它时登记表的代次。对不上就说明视图过期：在标记之下重新取一次 Docker 清单。
#[tokio::test]
async fn a_creation_that_finished_after_the_inventory_still_refuses_the_reclaim() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.path("logs/runner-a")).unwrap();
    let mounted = Arc::new(AtomicBool::new(false));
    let listed = Arc::clone(&mounted);
    // 重新取清单那一刻，标记必须已经打上
    let marks_when_relisted = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&marks_when_relisted);
    let daemon = fake_daemon_answer(move || {
        if listed.load(Ordering::SeqCst) {
            *seen.lock().unwrap() = Some(crate::log_directory::reclaim_marks().len());
            json!([bind("late-creation", REAL_A)])
        } else {
            json!([])
        }
    })
    .await;
    let runtime = runtime(daemon.address, tree.resolver());
    let reused = tree.path("logs/runner-a");

    // 清理器取清单：那时还没有这次创建
    let inventory = runtime
        .mounted_log_directories(std::slice::from_ref(&reused))
        .await
        .unwrap();
    assert!(inventory.mounted.is_empty());
    assert_eq!(list_requests(&daemon), 1);

    a_creation_finishing_after_the_inventory(&mounted, REAL_A).await;
    assert!(
        pending_bind_sources().is_empty(),
        "the registration was handed over to the container"
    );

    let refused = runtime.begin_log_directory_reclaim(&reused).await;

    assert!(
        matches!(refused, Err(ReclaimRefused::InUse(_))),
        "a container created after the inventory still holds the directory: {refused:?}"
    );
    assert_eq!(
        list_requests(&daemon),
        2,
        "Docker is re-read once the candidate is marked"
    );
    assert_eq!(
        *marks_when_relisted.lock().unwrap(),
        Some(1),
        "the re-read happens under the mark"
    );
}

/// 重新取的那份视图得留给**后面的**候选用。清单之后建出来的容器绑的不是当前这个候选，而是
/// 同一轮里排在后面的一个：当前候选该放行，后面那个必须被拒。只把新清单用在当前候选上，后面
/// 的候选就只对照登记表判定，看不到这个容器。
#[tokio::test]
async fn a_late_container_holds_the_later_candidates_too() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.path("logs/runner-a")).unwrap();
    std::fs::create_dir_all(tree.path("logs/runner-b")).unwrap();
    let mounted = Arc::new(AtomicBool::new(false));
    let listed = Arc::clone(&mounted);
    let daemon = fake_daemon_answer(move || {
        if listed.load(Ordering::SeqCst) {
            json!([bind("late-creation", "/host/base/logs/runner-b")])
        } else {
            json!([])
        }
    })
    .await;
    let runtime = runtime(daemon.address, tree.resolver());
    let (a, b) = (tree.path("logs/runner-a"), tree.path("logs/runner-b"));
    runtime
        .mounted_log_directories(&[a.clone(), b.clone()])
        .await
        .unwrap();

    a_creation_finishing_after_the_inventory(&mounted, "/host/base/logs/runner-b").await;

    // 当前这个候选没被绑：放行。刷新发生在这一次
    let _permit_a = runtime.begin_log_directory_reclaim(&a).await.unwrap();
    assert_eq!(list_requests(&daemon), 2);

    let refused = runtime.begin_log_directory_reclaim(&b).await;

    assert!(
        matches!(refused, Err(ReclaimRefused::InUse(_))),
        "the refreshed view must cover the later candidates as well: {refused:?}"
    );
    assert_eq!(
        list_requests(&daemon),
        2,
        "the later candidate is decided by the refreshed view, not by another request"
    );
}

/// 重新取的那份视图整轮接着用：登记表没再动，后面的候选不再多问 Docker；又动了才再取一次。
#[tokio::test]
async fn the_refreshed_view_is_reused_until_the_registry_moves_again() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.path("logs/runner-a")).unwrap();
    std::fs::create_dir_all(tree.path("logs/runner-b")).unwrap();
    let daemon = fake_daemon(json!([])).await;
    let runtime = runtime(daemon.address, tree.resolver());
    let (a, b) = (tree.path("logs/runner-a"), tree.path("logs/runner-b"));
    runtime
        .mounted_log_directories(&[a.clone(), b.clone()])
        .await
        .unwrap();
    assert_eq!(list_requests(&daemon), 1);

    move_the_registry();
    let permit_a = runtime.begin_log_directory_reclaim(&a).await.unwrap();
    assert_eq!(list_requests(&daemon), 2, "an expired view is refreshed");
    let permit_b = runtime.begin_log_directory_reclaim(&b).await.unwrap();
    assert_eq!(
        list_requests(&daemon),
        2,
        "nothing moved since the refresh, so Docker is not asked again"
    );
    drop((permit_a, permit_b));

    move_the_registry();
    runtime.begin_log_directory_reclaim(&a).await.unwrap();
    assert_eq!(list_requests(&daemon), 3);
}

/// 登记表没动过：这一轮的视图还作数，删前确认直接用它，不再问 Docker
#[tokio::test]
async fn an_unchanged_registry_costs_no_extra_request() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.path("logs/runner-a")).unwrap();
    let daemon = fake_daemon(json!([])).await;
    let runtime = runtime(daemon.address, tree.resolver());
    let reused = tree.path("logs/runner-a");
    runtime
        .mounted_log_directories(std::slice::from_ref(&reused))
        .await
        .unwrap();
    assert_eq!(list_requests(&daemon), 1);

    let _permit = runtime.begin_log_directory_reclaim(&reused).await.unwrap();

    assert_eq!(
        list_requests(&daemon),
        1,
        "the view taken for this sweep is still good"
    );
}

/// 视图过期了、Docker 却读不到：判不出来，不给许可，标记也不留着
#[tokio::test]
async fn an_expired_view_that_cannot_be_refreshed_refuses_the_reclaim() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.path("logs/runner-a")).unwrap();
    let daemon = fake_daemon(json!([])).await;
    let runtime = runtime(daemon.address, tree.resolver());
    let reused = tree.path("logs/runner-a");
    runtime
        .mounted_log_directories(std::slice::from_ref(&reused))
        .await
        .unwrap();
    move_the_registry();
    drop(daemon);

    let refused = runtime.begin_log_directory_reclaim(&reused).await;

    assert!(
        matches!(refused, Err(ReclaimRefused::Unconfirmed(_))),
        "{refused:?}"
    );
    assert!(
        crate::log_directory::reclaim_marks().is_empty(),
        "a refused permit leaves no mark behind"
    );
}

/// 许可只盖住它自己那一个目录：同一轮里别的候选不受影响，同一个目录不能同时删两次
#[tokio::test]
async fn a_permit_covers_only_its_own_directory() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.path("logs/runner-a")).unwrap();
    std::fs::create_dir_all(tree.path("logs/runner-b")).unwrap();
    let daemon = fake_daemon(json!([])).await;
    let runtime = runtime(daemon.address, tree.resolver());

    let _a = runtime
        .begin_log_directory_reclaim(&tree.path("logs/runner-a"))
        .await
        .unwrap();
    let _b = runtime
        .begin_log_directory_reclaim(&tree.path("logs/runner-b"))
        .await
        .unwrap();

    assert!(
        runtime
            .begin_log_directory_reclaim(&tree.path("logs/runner-a"))
            .await
            .is_err(),
        "the same directory cannot be reclaimed twice at once"
    );
}

/// 候选换算不到宿主机：判不出来，许可被拒（和清单阶段的"无法确认"一致）
#[tokio::test]
async fn an_unmappable_candidate_is_refused_the_reclaim() {
    let tree = Tree::new();
    let daemon = fake_daemon(json!([])).await;
    let runtime = runtime(daemon.address, tree.resolver());

    let refused = runtime
        .begin_log_directory_reclaim(Path::new("/elsewhere/runner-a"))
        .await;

    assert!(
        matches!(refused, Err(ReclaimRefused::Unconfirmed(_))),
        "{refused:?}"
    );
}
