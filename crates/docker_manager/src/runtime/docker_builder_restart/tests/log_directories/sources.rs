//! 原样重放的 bind 源核验：经这次恢复的挂载视图看。宿主机路径和 RCoder 容器内路径不一样的
//! 部署（Compose）也只有它看得见。视图说不上来时不拒绝。

use super::*;

/// 容器里一条普通的数据挂载，不是日志挂载
const NOTES_TARGET: &str = "/app/notes";

/// 给 `restart` 加一条数据挂载 [`NOTES_TARGET`]，源是 `source`
fn add_data_mount(restart: &mut LoggedRestart, source: &std::path::Path) -> String {
    let source = source.to_string_lossy().into_owned();
    restart
        .template
        .volumes
        .push(host_path_volume(NOTES_TARGET, &source));
    restart
        .archive
        .binds
        .push(format!("{source}:{NOTES_TARGET}"));
    source
}

/// 一条挂载视图判不出通向哪里的数据挂载：RCoder 进程那一侧，`visible/logs/container/linked`
/// 是指到挂载表之外（`outside`）的符号链接，源是它下面的 `data`。
///
/// `host_side_link` 为真时宿主机树里的那一级也是同样的链接：这条宿主机路径在本进程直接看得见
pub(super) fn add_unconfirmable_data_mount(
    restart: &mut LoggedRestart,
    outside: &std::path::Path,
    host_side_link: bool,
) -> String {
    let process_side = restart.visible_root();
    std::fs::create_dir_all(&process_side).unwrap();
    std::os::unix::fs::symlink(outside, process_side.join("linked")).unwrap();
    if host_side_link {
        std::os::unix::fs::symlink(outside, restart.host_root().join("linked")).unwrap();
    }
    std::fs::create_dir_all(outside.join("data")).unwrap();
    add_data_mount(restart, &restart.host_root().join("linked/data"))
}

/// 断言夹具确实是"挂载视图判不出来"，而不是"整条路径在挂载之外"
async fn assert_unconfirmable(restart: &LoggedRestart, daemon: &ScriptedDaemon, source: &str) {
    let runtime = restart.runtime(daemon.address);
    let admission = runtime.log_domain().await;
    let located = RealLocations::new(admission.space().unwrap(), &LocalFileSystem)
        .locate(std::path::Path::new(source))
        .await;
    assert!(
        matches!(located, Ok(Location::Unconfirmed(_))),
        "the fixture must be a source the mount view cannot confirm: {located:?}"
    );
}

/// 回归：挂载视图判不出这条源通向哪里（路径里有一级指到挂载之外的链接），但宿主机路径在本
/// 进程直接看得见。改动之前这样的部署恢复得了，现在也必须恢复得了——判不出来不拒绝重启
#[tokio::test]
async fn a_source_the_mount_view_cannot_confirm_is_accepted_when_its_host_path_is_visible() {
    let mut restart = LoggedRestart::new();
    let outside = tempfile::tempdir().unwrap();
    let source = add_unconfirmable_data_mount(&mut restart, outside.path(), true);
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;
    assert_unconfirmable(&restart, &daemon, &source).await;

    restart.restore(&daemon).await.unwrap_or_else(|error| {
        panic!("a bind source the mount view cannot confirm must not fail the restart: {error:?}")
    });

    daemon.assert_followed(5);
    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{source}:{NOTES_TARGET}"))),
        "the bind is replayed with its archived host path: {binds:?}"
    );
}

/// 几处都看不见才报确认不了：挂载视图判不出来，宿主机路径在本进程也看不到，又不在 RCoder 的
/// 工作区下面。不建替代容器，也不建新日志目录。报的是"确认不了"，不是"缺失"
#[tokio::test]
async fn a_source_no_view_can_see_is_reported_as_unconfirmed() {
    let mut restart = LoggedRestart::new();
    let outside = tempfile::tempdir().unwrap();
    let source = add_unconfirmable_data_mount(&mut restart, outside.path(), false);
    let daemon = scripted(absent_original()).await;
    assert_unconfirmable(&restart, &daemon, &source).await;

    let result = restart.restore(&daemon).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message))
            if message.contains("Cannot confirm restart bind source") && message.contains(&source)),
        "{result:?}"
    );
    daemon.assert_followed(2);
    let mut left: Vec<_> = std::fs::read_dir(restart.visible_root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [std::ffi::OsString::from("linked")],
        "a restart that does not happen creates no log directory"
    );
}

/// 挂载视图看得到这条源的位置，那里没有它：就是缺失。宿主机路径在本进程直接看得见也不算——
/// 视图里的路径换算回宿主机就是这条源，别处同名的目录是另一份数据
#[tokio::test]
async fn a_source_missing_in_the_mount_view_is_missing_even_if_its_host_path_is_visible() {
    let mut restart = LoggedRestart::new();
    std::fs::create_dir_all(restart.root.path().join("visible/logs/other")).unwrap();
    // 宿主机树里有这个目录；RCoder 进程那一侧对应的位置上没有
    let directory = restart.root.path().join("host/other/project-data");
    std::fs::create_dir_all(&directory).unwrap();
    add_data_mount(&mut restart, &directory);
    let daemon = scripted(absent_original()).await;

    let result = restart.restore(&daemon).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Restart bind source is missing")),
        "{result:?}"
    );
    daemon.assert_followed(2);
}

/// 源的位置上是一个文件：bind 要的是目录，同样按缺失报
#[tokio::test]
async fn a_file_where_the_bind_expects_a_directory_is_missing() {
    let mut restart = LoggedRestart::new();
    std::fs::create_dir_all(restart.root.path().join("visible/logs/other")).unwrap();
    let file = restart.root.path().join("host/other/notes");
    std::fs::create_dir_all(restart.root.path().join("host/other")).unwrap();
    std::fs::write(&file, b"not a directory").unwrap();
    add_data_mount(&mut restart, &file);
    std::fs::write(
        restart.root.path().join("visible/logs/other/notes"),
        b"not a directory",
    )
    .unwrap();
    let daemon = scripted(absent_original()).await;

    let result = restart.restore(&daemon).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Restart bind source is missing")),
        "{result:?}"
    );
    daemon.assert_followed(2);
}

/// 挂载视图读不出来——不是"在"也不是"不在"，是读取这一步出了错（这里用一个超出文件名长度上限
/// 的成分触发）。退回两处也看不见：报的是读取失败，不报缺失，也不放行
#[tokio::test]
async fn a_source_the_mount_view_cannot_read_is_reported_as_an_inspection_failure() {
    let mut restart = LoggedRestart::new();
    std::fs::create_dir_all(restart.root.path().join("visible/logs/other")).unwrap();
    let oversized = restart.root.path().join("host/other").join("a".repeat(300));
    add_data_mount(&mut restart, &oversized);
    let daemon = scripted(absent_original()).await;

    let result = restart.restore(&daemon).await;

    assert!(
        matches!(&result, Err(Error::DockerError(message)) if message.contains("Inspect restart bind source")),
        "{result:?}"
    );
    daemon.assert_followed(2);
}

/// 要换新目录的日志挂载不核验旧目录：它可能早被清理器回收了
#[tokio::test]
async fn a_reclaimed_log_directory_does_not_stop_the_restart() {
    let restart = LoggedRestart::new();
    std::fs::remove_dir_all(&restart.old_directory).unwrap();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(5);
    assert_ne!(
        restart.bound_log_name(&daemon, 4, LOG_TARGET),
        restart.old_name
    );
}
