//! 日志目录的占用登记跟着创建结果走：建好到 Docker 应答之间一直登记着；结果不明（应答丢、
//! 5xx、408、被取消）时保留；判不出在不在日志根里的 bind 也登记。续做时取不到路径空间也不让
//! 恢复失败。

use super::*;

/// 日志挂载的源判不出在不在日志根里（它经过一个指到 RCoder 挂载之外的符号链接），但归档能核实
/// 它是日志挂载：照样换新目录。日志目录是一次性的，旧的那个可能早已过期、清理器正在回收。
#[tokio::test]
async fn a_log_mount_that_cannot_be_classified_gets_a_new_directory() {
    let mut restart = LoggedRestart::new();
    // RCoder 进程里的 `visible/logs` 是真实目录；里面的 `alias` 指到挂载表之外的一个位置
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(restart.root.path().join("visible")).unwrap();
    std::fs::create_dir(restart.root.path().join("visible/logs")).unwrap();
    std::os::unix::fs::symlink(
        outside.path(),
        restart.root.path().join("visible/logs/alias"),
    )
    .unwrap();
    // 归档里记的 bind 源是宿主机树里的 `alias/<旧目录>`，Docker 解析这个链接
    let alias = restart
        .root
        .path()
        .join("host/alias")
        .to_string_lossy()
        .into_owned();
    let old_directory = format!("{}/{}", alias, restart.old_name);
    std::fs::create_dir_all(outside.path().join(&restart.old_name)).unwrap();
    std::fs::write(
        outside.path().join(&restart.old_name).join("old.log"),
        b"old",
    )
    .unwrap();
    for volume in &mut restart.template.volumes {
        if volume.name == LOG_TARGET {
            volume.uid = old_directory.clone();
        }
    }
    restart.archive.binds[0] = format!("{old_directory}:{LOG_TARGET}:rw,z");
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    // 绑的是 alias 下的新目录，不是旧目录
    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let bound = binds
        .iter()
        .filter_map(Value::as_str)
        .find_map(|bind| {
            bind.split_once(&format!(":{LOG_TARGET}"))
                .map(|(source, _)| source.to_owned())
        })
        .unwrap_or_default();
    let name = std::path::Path::new(&bound)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        bound.starts_with(&alias),
        "a log mount is swapped even when it cannot be classified: {binds:?}"
    );
    assert!(
        name.starts_with("rcoder-app-builder-265-") && name != restart.old_name,
        "{bound}"
    );
    assert!(
        outside.path().join(&name).is_dir(),
        "the new directory is created next to the old one"
    );
    assert_eq!(
        std::fs::read_dir(outside.path()).unwrap().count(),
        2,
        "the old directory is left alone; only a new one is added"
    );
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &bound)])
    );
}

/// 判不出来时换新目录只是尽量：那个位置被别的挂载挡住、RCoder 看不到，新目录建不出来，就照
/// 原样重放并登记旧目录。不让恢复失败。
#[tokio::test]
async fn an_evidenced_log_mount_that_cannot_get_a_new_directory_is_replayed() {
    let mut restart = LoggedRestart::new();
    // 宿主机 `host/hidden` 在 RCoder 里被另一条挂载挡住：同一个容器内路径对应别的宿主机目录
    restart.extra_mounts.push((
        restart.root.path().join("visible/logs/hidden"),
        restart.root.path().join("host/elsewhere"),
    ));
    let hidden = restart.root.path().join("host/hidden");
    std::fs::create_dir_all(hidden.join(&restart.old_name)).unwrap();
    let old_directory = hidden
        .join(&restart.old_name)
        .to_string_lossy()
        .into_owned();
    for volume in &mut restart.template.volumes {
        if volume.name == LOG_TARGET {
            volume.uid = old_directory.clone();
        }
    }
    restart.archive.binds[0] = format!("{old_directory}:{LOG_TARGET}:rw,z");
    let seen_at_create = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen_at_create);
    let daemon = scripted_with_hook(
        creation_script(step("POST", "/containers/create", 200, created())),
        Arc::new(move |request: &str| {
            if request.starts_with("POST") && request.contains("/containers/create") {
                recorder
                    .lock()
                    .unwrap()
                    .extend(crate::log_directory::pending_bind_sources());
            }
        }),
    )
    .await;

    let replacement = restart.restore(&daemon).await.unwrap_or_else(|error| {
        panic!("a log mount that cannot get a new directory must not fail the restart: {error:?}")
    });

    // 绑的还是旧目录，没有换源记录
    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{old_directory}:{LOG_TARGET}:rw,z"))),
        "{binds:?}"
    );
    assert!(daemon.body(4)["Labels"][RESTART_LOG_DIR].is_null());
    assert_eq!(replacement.expected_volumes, restart.template.volumes);
    // 旧目录的源登记到替代容器建出来
    let seen = seen_at_create.lock().unwrap().clone();
    assert!(seen.contains(&PathBuf::from(&old_directory)), "{seen:?}");
}

/// 日志目录建好之后、创建请求得到应答之前，一直登记为正在准备挂载。
#[tokio::test]
async fn a_fresh_log_directory_stays_claimed_while_the_create_request_is_in_flight() {
    let restart = LoggedRestart::new();
    let seen_at_create = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen_at_create);
    let daemon = scripted_with_hook(
        creation_script(step("POST", "/containers/create", 200, created())),
        Arc::new(move |request: &str| {
            if request.starts_with("POST") && request.contains("/containers/create") {
                recorder
                    .lock()
                    .unwrap()
                    .extend(crate::log_directory::pending_bind_sources());
            }
        }),
    )
    .await;

    restart.restore(&daemon).await.unwrap();

    let name = restart.bound_log_name(&daemon, 4, LOG_TARGET);
    assert_eq!(
        seen_at_create.lock().unwrap().as_slice(),
        [PathBuf::from(new_log_directory(&restart, &name))],
        "while the request is in flight the directory is claimed, so the cleaner treats it as mounted"
    );
    assert!(
        !crate::log_directory::pending_bind_sources()
            .contains(&PathBuf::from(new_log_directory(&restart, &name))),
        "once Docker has answered, the inventory alone protects the directory"
    );
}

/// 归档里有一条 bind 判不出在不在日志根里（源经过一个指向自己的符号链接）：不让恢复失败。
/// 它不在换源计划里，照原样重放；它的源和新目录一样，登记到创建请求得到应答为止。
#[tokio::test]
async fn a_bind_that_cannot_be_classified_is_replayed_and_registered() {
    let mut restart = LoggedRestart::new();
    // 让 RCoder 进程看得到宿主机那棵树：树本身挪到 `visible/logs`，`host` 变成指向它的链接；
    // 树里放一个指向自己的链接
    std::fs::create_dir(restart.root.path().join("visible")).unwrap();
    std::fs::rename(
        restart.root.path().join("host"),
        restart.root.path().join("visible/logs"),
    )
    .unwrap();
    std::os::unix::fs::symlink("visible/logs", restart.root.path().join("host")).unwrap();
    std::os::unix::fs::symlink("loop", restart.root.path().join("visible/logs/loop")).unwrap();
    let notes = restart
        .root
        .path()
        .join("host/loop/shared")
        .to_string_lossy()
        .into_owned();
    restart.archive.binds.push(format!("{notes}:/app/notes"));
    let seen_at_create = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen_at_create);
    let daemon = scripted_with_hook(
        creation_script(step("POST", "/containers/create", 200, created())),
        Arc::new(move |request: &str| {
            if request.starts_with("POST") && request.contains("/containers/create") {
                recorder
                    .lock()
                    .unwrap()
                    .extend(crate::log_directory::pending_bind_sources());
            }
        }),
    )
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    // 日志挂载换成新目录；判不出来的那条照原样重放
    let name = restart.bound_log_name(&daemon, 4, LOG_TARGET);
    let new_directory = new_log_directory(&restart, &name);
    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{notes}:/app/notes"))),
        "a bind that cannot be classified is replayed as archived: {binds:?}"
    );
    assert_eq!(
        restart.bound_sources(&daemon, 4),
        json!({ LOG_TARGET: new_directory })
            .as_object()
            .cloned()
            .unwrap()
    );
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &new_directory)])
    );
    // 换源记录里只有日志挂载
    assert_eq!(
        restart.bound_sources(&daemon, 4),
        json!({ LOG_TARGET: new_directory })
            .as_object()
            .cloned()
            .unwrap()
    );
    // 创建请求在途时，新目录和判不出来的那条源都登记着
    let mut seen = seen_at_create.lock().unwrap().clone();
    seen.sort();
    let mut expected = vec![PathBuf::from(new_directory), PathBuf::from(&notes)];
    expected.sort();
    assert_eq!(
        seen, expected,
        "while the request is in flight both are claimed, so the cleaner keeps every candidate"
    );
    assert!(
        !crate::log_directory::pending_bind_sources().contains(&PathBuf::from(&notes)),
        "once Docker has answered, the inventory alone protects it"
    );
}

/// 续做时取不到路径空间（定不出换源计划）：不让恢复失败。label 的每一项都是归档里某条
/// HostPath 挂载的同级新目录就行。
#[cfg(not(feature = "deploy-host"))]
#[tokio::test]
async fn a_resumed_replacement_is_accepted_even_without_a_mount_table() {
    let restart = LoggedRestart::new();
    let log_directory = new_log_directory(&restart, "rcoder-app-builder-265-20251026090000");
    let labels = json!({
        RESTORE: restart.template.archive.uid.clone(),
        RESTART_LOG_DIR: json!({ LOG_TARGET: log_directory }).to_string(),
    });
    let daemon = scripted(vec![
        // 自检从 ping 开始；测试进程不在容器里，拿不到自己的容器 ID，等于取不到路径空间
        step("GET", "_ping", 200, json!("OK")),
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(&restart.template, labels),
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
            panic!("an unavailable mount table must not block the resume: {error}")
        });

    daemon.assert_followed(3);
    assert_eq!(replacement.id, REPLACEMENT_UID);
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &log_directory)])
    );
}

/// 取不到路径空间时也不什么都信：label 里的目录得是归档里某条 HostPath 挂载的同级新目录。
#[cfg(not(feature = "deploy-host"))]
#[tokio::test]
async fn without_a_mount_table_a_resumed_label_still_has_to_name_fresh_siblings() {
    let restart = LoggedRestart::new();
    let elsewhere = restart
        .root
        .path()
        .join("elsewhere/log-directory")
        .to_string_lossy()
        .into_owned();
    for (name, sources) in [
        (
            "a directory that is not a sibling of the archived one",
            json!({ LOG_TARGET: elsewhere }),
        ),
        (
            "the archived directory itself",
            json!({ LOG_TARGET: restart.old_directory }),
        ),
        (
            "a mount the archived builder does not have",
            json!({ "/app/unknown": new_log_directory(&restart, "rcoder-app-builder-265-20251026090000") }),
        ),
    ] {
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
                        RESTART_LOG_DIR: sources.to_string(),
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

        let refused = runtime
            .replace_builder_container(
                &restart.template,
                &restart.archive,
                original(&restart.template),
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("{name} must be refused"));

        assert!(
            format!("{refused}").contains("Restart target"),
            "{name}: {refused}"
        );
    }
}

/// daemon 以 4xx 明确拒绝：替代容器确实没建成，刚建的日志目录没人用，删掉。
#[tokio::test]
async fn new_log_directories_are_discarded_when_the_replacement_is_explicitly_rejected() {
    let restart = LoggedRestart::new();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        400,
        json!({"message": "bad parameter"}),
    )))
    .await;

    let result = restart.restore(&daemon).await;

    // 明确拒绝保留结构化的类型和状态码：上层据此按"确实被拒绝"归类
    assert!(
        matches!(&result, Err(Error::RequestRejected(rejection)) if rejection.status == 400 && rejection.message.contains("bad parameter")),
        "{result:?}"
    );
    daemon.assert_followed(5);
    assert_eq!(
        std::fs::read_dir(restart.visible_root()).unwrap().count(),
        0,
        "directories made for a replacement that was never created are removed"
    );
}

/// 创建请求还没发出去就失败了（读取 daemon 信息被拒）：同样确认没建成，删掉。
#[tokio::test]
async fn new_log_directories_are_discarded_when_the_request_was_never_sent() {
    let restart = LoggedRestart::new();
    let mut script = absent_original();
    script.extend([
        step("GET", "/images/", 200, image_inspect()),
        step("GET", "/info", 500, json!({"message": "daemon refused"})),
    ]);
    let daemon = scripted(script).await;

    let result = restart.restore(&daemon).await;

    assert!(result.is_err(), "{result:?}");
    daemon.assert_followed(4);
    assert_eq!(
        std::fs::read_dir(restart.visible_root()).unwrap().count(),
        0,
        "nothing was sent to Docker, so the directories are certainly unused"
    );
}

async fn assert_kept_after_unknown_outcome(result: Result<Replacement>, restart: &LoggedRestart) {
    assert!(result.is_err(), "{result:?}");
    let mut fresh: Vec<String> = std::fs::read_dir(restart.visible_root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
        .collect();
    fresh.sort();
    assert_eq!(
        fresh.len(),
        1,
        "the replacement may exist and bind this directory; it must be left alone"
    );
    // 登记同样留着：Docker 可能还在处理这次创建，清单里未必已经有它。到时间才由登记自己过期
    assert!(
        crate::log_directory::pending_bind_sources()
            .contains(&PathBuf::from(new_log_directory(restart, &fresh[0]))),
        "an unknown outcome must keep the directory claimed until the claim expires"
    );
}

/// 408、499 看着是 4xx，其实是超时和客户端断开：请求可能已经生效，按结果不明处理。
#[tokio::test]
async fn new_log_directories_are_kept_when_the_daemon_answers_408() {
    let restart = LoggedRestart::new();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        408,
        json!({"message": "request timeout"}),
    )))
    .await;

    assert_kept_after_unknown_outcome(restart.restore(&daemon).await, &restart).await;
}

/// 请求发出去了、daemon 报 5xx：结果不明，替代容器可能已经建出来，目录原样留着。
#[tokio::test]
async fn new_log_directories_are_kept_when_the_daemon_fails_after_the_request() {
    let restart = LoggedRestart::new();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        500,
        json!({"message": "driver failed"}),
    )))
    .await;

    assert_kept_after_unknown_outcome(restart.restore(&daemon).await, &restart).await;
}

/// 请求发出去了、应答是成功却没有 ID：容器可能已经建出来，目录原样留着。
#[tokio::test]
async fn new_log_directories_are_kept_when_the_reply_carries_no_id() {
    let restart = LoggedRestart::new();
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        201,
        json!({"Id": "", "Warnings": []}),
    )))
    .await;

    assert_kept_after_unknown_outcome(restart.restore(&daemon).await, &restart).await;
}

/// 请求发出去了、应答丢了：结果不明，目录原样留着。
#[tokio::test]
async fn new_log_directories_are_kept_when_the_reply_is_lost() {
    let restart = LoggedRestart::new();
    let daemon = scripted(creation_script(lost_reply("POST", "/containers/create"))).await;

    assert_kept_after_unknown_outcome(restart.restore(&daemon).await, &restart).await;
}

/// 恢复流程等到创建应答时被取消：请求已经发出去了，登记转去"结果不明"留着，目录原样不动。
#[tokio::test]
async fn a_cancelled_restore_keeps_the_directory_claimed() {
    let restart = LoggedRestart::new();
    let mut script = absent_original();
    script.extend(creation_prelude(stalled("POST", "/containers/create")));
    let daemon = scripted(script).await;

    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        restart.restore(&daemon),
    )
    .await;

    assert!(outcome.is_err(), "the create request must not be answered");
    daemon.assert_followed(5);
    let mut fresh: Vec<String> = std::fs::read_dir(restart.visible_root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
        .collect();
    fresh.sort();
    assert_eq!(fresh.len(), 1, "the directory is left as it was");
    assert!(
        crate::log_directory::pending_bind_sources()
            .contains(&PathBuf::from(new_log_directory(&restart, &fresh[0]))),
        "a cancelled request was sent: its outcome is unknown, so the claim must stay"
    );
}
