//! 登记表：在途登记、代次、删除标记。
//!
//! 两边的判定不在这里——清理器核对"有没有登记落在被删目录里"、创建方等"要绑的目录"删完——
//! 都在 `runtime::docker_runtime::log_dir_mounts`。

use super::*;
use crate::log_directory::{PendingBinds, reclaim_marks, registry_view};

const HOST: &str = "/srv/logs/container/runner-a";
const PROCESS: &str = "/app/logs/container/runner-a";

fn mark(host: &str) -> (ReclaimMark, RegistryView) {
    mark_reclaim(Path::new(host), Path::new(PROCESS)).unwrap()
}

/// 同一个目录只许一次删除在删：第二个标记被拒绝
#[test]
fn a_directory_cannot_be_marked_twice() {
    let (first, _) = mark(HOST);

    assert!(
        mark_reclaim(Path::new(HOST), Path::new(PROCESS)).is_err(),
        "another deletion is already deleting this directory"
    );

    drop(first);
    assert!(mark_reclaim(Path::new(HOST), Path::new(PROCESS)).is_ok());
}

/// 标记按归一化之后的宿主机路径认；进程路径原样留着，创建方按目录身份判冲突时要用
#[test]
fn a_mark_keeps_both_names_of_the_directory() {
    let (first, _) = mark("/srv/logs/container/./runner-a");

    assert!(mark_reclaim(Path::new(HOST), Path::new(PROCESS)).is_err());
    assert_eq!(
        reclaim_marks(),
        [(PathBuf::from(HOST), PathBuf::from(PROCESS))]
    );

    drop(first);
    assert!(reclaim_marks().is_empty(), "dropping the mark releases it");
}

/// 打标记和读表是同一次持锁：返回的视图就是打上标记那一刻的登记和代次
#[test]
fn marking_returns_the_registry_as_of_that_moment() {
    let mut guard = PendingBinds::default();
    guard.claim_blocking(Path::new(HOST));
    let before = registry_view().generation;

    let (_mark, view) = mark(HOST);

    assert_eq!(view.sources, [PathBuf::from(HOST)]);
    assert_eq!(view.generation, before, "marking itself does not move it");
    drop(guard);
}

/// 代次：登记加上、最后一个守卫注销，各加一。同一条路径还有别的守卫时，先退出的不动代次——
/// 登记还在，清理器读得到
#[test]
fn every_registration_and_every_release_advances_the_generation() {
    let start = registry_view().generation;
    let mut first = PendingBinds::default();
    first.claim_blocking(Path::new(HOST));
    assert_eq!(registry_view().generation, start + 1);

    let mut second = PendingBinds::default();
    second.claim_blocking(Path::new(HOST));
    assert_eq!(registry_view().generation, start + 2);

    drop(first);
    assert_eq!(
        registry_view().generation,
        start + 2,
        "one of two guards leaving keeps the registration"
    );
    assert_eq!(registry_view().sources, [PathBuf::from(HOST)]);

    drop(second);
    assert_eq!(registry_view().generation, start + 3);
    assert!(registry_view().sources.is_empty());
}

/// 请求发出后结果不明的登记不占在途表：它转去带过期时间的那张表，代次照常往前走
#[tokio::test]
async fn a_registration_with_an_unknown_outcome_leaves_the_in_flight_table() {
    let start = registry_view().generation;
    let mut binds = PendingBinds::default();
    binds.claim_blocking(Path::new(HOST));
    binds.request_sent();

    drop(binds);

    assert_eq!(registry_view().generation, start + 2);
    assert!(view().sources.is_empty(), "nothing is in flight any more");
    // 它没消失：转去了带过期时间的那张表，清理器照样读得到
    assert_eq!(
        crate::log_directory::unresolved_sources(),
        [PathBuf::from(HOST)]
    );
}
