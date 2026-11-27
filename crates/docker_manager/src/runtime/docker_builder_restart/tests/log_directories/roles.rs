//! 哪些挂载换、换源记录怎么核：换源记录里不能出现持久数据和不在日志挂载记录里的挂载；
//! 记录里没有的挂载和归档一模一样；取不到挂载表时读换源记录不拦。数据挂载在 `data_mounts`。

use super::*;

/// 日志挂载的源确定在日志根之外：不换。换的理由只有一个——旧目录可能被清理器回收；清理器碰不到
/// 日志根外面的目录，照原样绑回去是安全的。换了反而每次重启多留一个没人回收的目录，容器里还
/// 看不到上一次的日志。
///
/// 源放在 RCoder 看得到的卷上、只是不在清理目录之下（`cleanup_config.log_cleanup.log_dir` 没盖住
/// 日志挂载源的形态）：同级新目录是建得出来的，所以"不换"必须是代码的决定，不是建不出来的副作用。
///
/// 回归：第三轮加"必须换"标志的时候丢了这一条，日志根外的日志挂载跟着换了一个多版本。
#[tokio::test]
async fn a_log_mount_outside_the_log_root_keeps_its_directory() {
    let mut restart = LoggedRestart::new();
    // `host/container` 是日志根；`host/other` 和它在同一块卷上，却不在日志根之下
    let outside = restart.root.path().join("host/other");
    std::fs::create_dir_all(outside.join(&restart.old_name)).unwrap();
    std::fs::write(
        outside.join(&restart.old_name).join("last-time.log"),
        b"kept",
    )
    .unwrap();
    let old_directory = outside
        .join(&restart.old_name)
        .to_string_lossy()
        .into_owned();
    restart.expose_bind_source(&old_directory);
    for volume in &mut restart.template.volumes {
        if volume.name == LOG_TARGET {
            volume.uid = old_directory.clone();
        }
    }
    restart.archive.binds[0] = format!("{old_directory}:{LOG_TARGET}:rw,z");
    restart.re_record(LOG_TARGET, &old_directory);
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(5);
    let body = daemon.body(4);
    let binds = body["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{old_directory}:{LOG_TARGET}:rw,z"))),
        "the log mount keeps its directory: {binds:?}"
    );
    // 没有新建目录，也没有换源记录：上一次的日志容器里照旧看得到
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        1,
        "no new directory is left behind"
    );
    assert!(body["Labels"][RESTART_LOG_DIR].is_null());
    // 日志挂载记录原样带过去：下次重启它，照样认得出
    assert_eq!(
        body["Labels"][LOG_MOUNTS_LABEL]
            .as_str()
            .and_then(|label| serde_json::from_str::<Value>(label).ok()),
        Some(json!([{ "target": LOG_TARGET, "source": old_directory }]))
    );
    assert_eq!(replacement.expected_volumes, restart.template.volumes);
}

/// 日志挂载在日志根下更深处：替代容器的新目录建在同一层，日志根下那第一级目录的修改时间
/// 不会自己变，清理器会一直把它当过期候选。恢复时把它刷新到现在，清理器删前的复查才看得出
/// "刚被用过"。
#[tokio::test]
async fn a_nested_log_directory_refreshes_the_directory_the_cleaner_scans() {
    let mut restart = LoggedRestart::new();
    restart.nest_log_mount("tenant/logs");
    // 清理器按 RCoder 进程里的路径扫日志根；先把那第一级目录做成早就过期的样子
    let scanned = restart.visible_root().join("tenant");
    std::fs::create_dir_all(scanned.join("logs")).unwrap();
    let long_ago = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(60);
    std::fs::File::open(&scanned)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    restart.restore(&daemon).await.unwrap();

    let refreshed = std::fs::metadata(&scanned).unwrap().modified().unwrap();
    assert!(
        refreshed
            .duration_since(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .is_ok(),
        "the directory the cleaner scans must look recently used, not expired since {long_ago:?}: {refreshed:?}"
    );
    assert_eq!(
        std::fs::read_dir(scanned.join("logs")).unwrap().count(),
        1,
        "the replacement's new log directory lives next to where the old one was"
    );
}

/// 记录里只有一条日志挂载（另一条建替代容器的时候判不出来、原样重放了）：记录里有的按记录，
/// 没有的必须和归档一模一样。替代容器已经存在，它绑什么就是什么，不为难恢复。
#[tokio::test]
async fn a_mount_the_record_does_not_name_stays_on_its_archived_directory() {
    let restart = LoggedRestart::with_audit_log();
    let fresh = new_log_directory(&restart, "rcoder-app-builder-265-20251026090000");
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &restart.template,
                json!({
                    RESTORE: restart.template.archive.uid.clone(),
                    RESTART_LOG_DIR: json!({
                        LOG_TARGET: new_log_directory(
                            &restart,
                            "rcoder-app-builder-265-20251026090000"
                        )
                    })
                    .to_string(),
                }),
            ),
        ),
    ])
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(2);
    assert_eq!(replacement.id, REPLACEMENT_UID);
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &fresh)])
    );
}

/// label 里不能出现工作区：`/home/user` 下面是持久数据，永不换源。多出来的键会改写那条挂载的
/// 期望值，那正是这份核验要拦的。
#[tokio::test]
async fn a_resumed_label_that_names_the_workspace_is_refused() {
    let restart = LoggedRestart::new();
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &restart.template,
                json!({
                    RESTORE: restart.template.archive.uid.clone(),
                    RESTART_LOG_DIR: json!({
                        LOG_TARGET: new_log_directory(
                            &restart,
                            "rcoder-app-builder-265-20251026090000"
                        ),
                        WORKSPACE_TARGET: "/srv/elsewhere/workspace"
                    })
                    .to_string(),
                }),
            ),
        ),
    ])
    .await;

    let result = restart.restore(&daemon).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("which holds persistent data and is never replaced")),
        "{result:?}"
    );
    daemon.assert_followed(2);
}

/// label 里出现了一条既不是日志挂载、也不在日志根里的挂载：换它没有依据，拒绝。
#[tokio::test]
async fn a_resumed_label_that_names_an_auxiliary_mount_is_refused() {
    let restart = LoggedRestart::new();
    let assets = tempfile::tempdir().unwrap();
    let assets_source = assets.path().to_string_lossy().into_owned();
    let mut template = restart.template.clone();
    template
        .volumes
        .push(host_path_volume("/app/assets", &assets_source));
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &template,
                json!({
                    RESTORE: template.archive.uid.clone(),
                    RESTART_LOG_DIR: json!({
                        LOG_TARGET: new_log_directory(&restart, "rcoder-app-builder-265-20251026090000"),
                        "/app/assets": assets.path().with_file_name("assets-elsewhere").to_string_lossy().into_owned(),
                    })
                    .to_string(),
                }),
            ),
        ),
    ])
    .await;
    let runtime = restart.runtime(daemon.address);

    let result = runtime
        .replace_builder_container(&template, &restart.archive, original(&template))
        .await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("which is not a log mount")),
        "{result:?}"
    );
    daemon.assert_followed(2);
}

/// 取不到挂载表时读换源记录：判不出来不拦。记录里有一条日志挂载——挂载点不带 `container-logs`、
/// 环境变量也没记它，认它靠的是创建时的日志挂载记录（建替代容器的时候判它在日志根里，所以换了），
/// 续做照样认。
#[cfg(not(feature = "deploy-host"))]
#[tokio::test]
async fn without_a_mount_table_a_recorded_swap_is_accepted_for_a_mount_the_env_does_not_name() {
    let mut restart = LoggedRestart::new();
    // 它在日志挂载记录里，环境变量一个都没提它
    let old_cache = restart.host_root().join("cache-old");
    std::fs::create_dir_all(&old_cache).unwrap();
    let old_cache = old_cache.to_string_lossy().into_owned();
    restart
        .template
        .volumes
        .push(host_path_volume("/app/cache", &old_cache));
    restart
        .archive
        .binds
        .push(format!("{old_cache}:/app/cache"));
    restart
        .archive
        .log_mounts
        .as_mut()
        .unwrap()
        .push(log_mount("/app/cache", &old_cache));
    let fresh_log = new_log_directory(&restart, "rcoder-app-builder-265-20251026090000");
    let fresh_cache = new_log_directory(&restart, "rcoder-app-builder-265-20251026091000");
    let daemon = scripted(vec![
        // 自检从 ping 开始；测试进程不在容器里，拿不到自己的容器 ID，等于取不到挂载表
        step("GET", "_ping", 200, json!("OK")),
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &restart.template,
                json!({
                    RESTORE: restart.template.archive.uid.clone(),
                    RESTART_LOG_DIR: json!({
                        LOG_TARGET: fresh_log,
                        "/app/cache": fresh_cache,
                    })
                    .to_string(),
                }),
            ),
        ),
    ])
    .await;
    let runtime = DockerRuntime::new(docker_manager_configured(
        daemon.address,
        crate::DockerManagerConfig {
            log_reclaim_root: restart.visible_root(),
            ..Default::default()
        },
    ));

    let replacement = runtime
        .replace_builder_container(
            &restart.template,
            &restart.archive,
            original(&restart.template),
        )
        .await
        .unwrap_or_else(|error| {
            panic!("an unavailable mount table must not block the resume: {error:?}")
        });

    daemon.assert_followed(3);
    assert_eq!(replacement.id, REPLACEMENT_UID);
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &fresh_log), ("/app/cache", &fresh_cache)])
    );
}

/// 同一个换源记录、归档里却没有这条日志挂载的记录：换它没有依据，拒绝。取不到挂载表也一样拒——
/// 认角色只看归档，不依赖挂载表。
#[cfg(not(feature = "deploy-host"))]
#[tokio::test]
async fn without_a_mount_table_a_swap_of_an_unrecorded_mount_is_refused() {
    let mut restart = LoggedRestart::new();
    // 名字长得就是日志目录的样子，还带着时间戳。它不在记录里：不是 RCoder 建的
    let old_data = restart.host_root().join("customer-data-20251026080000");
    std::fs::create_dir_all(&old_data).unwrap();
    let old_data = old_data.to_string_lossy().into_owned();
    restart
        .template
        .volumes
        .push(host_path_volume("/mnt/project", &old_data));
    restart
        .archive
        .binds
        .push(format!("{old_data}:/mnt/project"));
    let daemon = scripted(vec![
        step("GET", "_ping", 200, json!("OK")),
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &restart.template,
                json!({
                    RESTORE: restart.template.archive.uid.clone(),
                    RESTART_LOG_DIR: json!({
                        LOG_TARGET: new_log_directory(&restart, "rcoder-app-builder-265-20251026090000"),
                        "/mnt/project": new_log_directory(&restart, "customer-data-20251026090000"),
                    })
                    .to_string(),
                }),
            ),
        ),
    ])
    .await;
    let runtime = DockerRuntime::new(docker_manager_configured(
        daemon.address,
        crate::DockerManagerConfig {
            log_reclaim_root: restart.visible_root(),
            ..Default::default()
        },
    ));

    let result = runtime
        .replace_builder_container(
            &restart.template,
            &restart.archive,
            original(&restart.template),
        )
        .await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("which is not a log mount")),
        "{result:?}"
    );
    daemon.assert_followed(3);
}

/// 回归（Codex 第七轮 P1）：源确定在日志根之外的日志挂载照原样重放，重放之前要核对它还在。
/// Compose 部署里宿主机路径和 RCoder 容器内路径不一样，宿主机路径在本进程并不存在——只有经
/// 挂载表反查才看得见它。只看本进程的同名路径会误报缺失，恢复一直卡在这里
#[tokio::test]
async fn an_outside_log_visible_through_the_mount_table_can_be_replayed_in_compose() {
    use crate::runtime::docker_runtime::host_path_space::{LogDomainVerdict, viewing};
    let mut restart = LoggedRestart::new();
    let daemon_source = restart
        .root
        .path()
        .join("host/other")
        .join(&restart.old_name);
    let process_source = restart
        .root
        .path()
        .join("visible/logs/other")
        .join(&restart.old_name);
    std::fs::create_dir_all(&process_source).unwrap();
    std::fs::write(process_source.join("previous.log"), b"kept").unwrap();
    assert!(
        !daemon_source.exists(),
        "the host's pathname must not be present in the RCoder container"
    );
    let old = daemon_source.to_str().unwrap();
    restart
        .template
        .volumes
        .iter_mut()
        .find(|volume| volume.name == LOG_TARGET)
        .unwrap()
        .uid = old.to_owned();
    restart.archive.binds[0] = format!("{old}:{LOG_TARGET}:rw,z");
    restart.re_record(LOG_TARGET, old);
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;
    let runtime = restart.runtime(daemon.address);
    let admission = runtime.log_domain().await;
    assert_eq!(
        admission.classify(LOG_TARGET, old).await,
        LogDomainVerdict::Outside
    );
    assert_eq!(
        viewing(admission.space().unwrap(), &daemon_source)
            .visible_path()
            .as_deref(),
        Some(process_source.as_path())
    );
    restart
        .restore(&daemon)
        .await
        .expect("the archived log still exists through RCoder's mount table");
    assert!(
        daemon.body(4)["HostConfig"]["Binds"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("{old}:{LOG_TARGET}:rw,z")))
    );
}
