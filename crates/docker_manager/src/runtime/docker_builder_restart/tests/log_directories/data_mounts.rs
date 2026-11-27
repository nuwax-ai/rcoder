//! 数据挂载永不换成别的目录，哪怕它落在日志根里、名字和挂载点长得都像日志目录：工作区是
//! 持久数据；自定义数据目录不在创建时的日志挂载记录里。照常恢复、原样重放，源登记到替代
//! 容器建出来。

use super::*;

/// 日志根的配置盖住了工作区（旧容器、改过日志根的部署）：工作区是持久数据，不换。替代容器
/// 绑的还是原来那个目录；日志挂载照换。工作区的源登记到替代容器建出来。
#[tokio::test]
async fn a_workspace_under_the_log_root_keeps_its_directory() {
    let mut restart = LoggedRestart::new();
    restart.move_workspace_under_log_root();
    std::fs::write(restart.workspace.path().join("keep"), b"data").unwrap();
    let workspace = restart.workspace.path().to_string_lossy().into_owned();
    restart.expose_bind_source(&workspace);
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

    let name = restart.bound_log_name(&daemon, 4, LOG_TARGET);
    let new_directory = new_log_directory(&restart, &name);
    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{workspace}:{WORKSPACE_TARGET}"))),
        "the workspace keeps its directory: {binds:?}"
    );
    assert_eq!(
        restart.bound_sources(&daemon, 4),
        json!({ LOG_TARGET: new_directory })
            .as_object()
            .cloned()
            .unwrap(),
        "the workspace is never recorded as replaced"
    );
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &new_directory)])
    );
    // 工作区的源登记着：它落在日志根里，是清理器的过期候选
    let mut seen = seen_at_create.lock().unwrap().clone();
    seen.sort();
    let mut expected = vec![PathBuf::from(new_directory), PathBuf::from(&workspace)];
    expected.sort();
    assert_eq!(seen, expected);
    assert!(restart.workspace.path().join("keep").is_file());
}

/// 旧版本保留的停止容器还在、工作区落在日志根里：工作区的源在删旧容器**之前**就登记，一直到
/// 替代容器建出来。旧容器一删它就不在任何容器的挂载里了，到建替代容器之间还要确认镜像。
#[tokio::test]
#[allow(unsafe_code)] // 测试内设置进程环境变量；nextest 每个用例独立进程
async fn data_under_the_log_root_is_registered_before_the_old_container_is_removed() {
    let receipts = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RCODER_OPERATION_RECEIPT_ROOT", receipts.path());
    }
    let mut restart = LoggedRestart::new();
    restart.move_workspace_under_log_root();
    // 真实归档里的卷按挂载点排好，和从容器读回的见证同序
    restart
        .template
        .volumes
        .sort_by(|a, b| (&a.name, &a.uid).cmp(&(&b.name, &b.uid)));
    let workspace = restart.workspace.path().to_string_lossy().into_owned();
    restart.expose_bind_source(&workspace);
    crate::runtime::docker_compute_receipt::save_stop(&restart.template.source)
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let daemon = scripted_with_hook(
        vec![
            step(
                "GET",
                ORIGINAL_UID,
                200,
                retained_original(&restart.template),
            ),
            step("GET", "/images/", 200, image_inspect()),
            step("DELETE", ORIGINAL_UID, 204, json!({})),
            step("GET", ORIGINAL_UID, 404, no_such_container()),
            step("GET", "rcoder-app-builder-", 404, no_such_container()),
            step("GET", "/images/", 200, image_inspect()),
            step("GET", "/info", 200, daemon_info()),
            step("POST", "/containers/create", 200, created()),
        ],
        Arc::new(move |request: &str| {
            let deleting = request.starts_with("DELETE");
            let preparing_image = request.starts_with("GET") && request.contains("/images/");
            let creating = request.starts_with("POST") && request.contains("/containers/create");
            if deleting || preparing_image || creating {
                recorder.lock().unwrap().push((
                    request.split_whitespace().next().unwrap_or("").to_owned(),
                    crate::log_directory::pending_bind_sources(),
                ));
            }
        }),
    )
    .await;

    restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(8);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "{seen:?}");
    for (request, sources) in seen.iter() {
        assert!(
            sources.contains(&PathBuf::from(&workspace)),
            "{request}: the data directory is nobody's mount once the old container is gone: {sources:?}"
        );
    }
}

/// 回归（Codex 第四、五轮 P1）：一条自定义的数据挂载落在日志根下面，源里有数据，目录名还带着
/// 时间戳、长得就是日志目录的样子；归档的日志环境变量指向真正的日志挂载。
///
/// 只按源路径挑换源对象，或者把目录名的格式当证据，替代容器绑的都是新建的空目录，数据就丢了。
/// 认角色只认创建时的日志挂载记录：它不在记录里，永不换源。真正的日志挂载在记录里，照换。
#[tokio::test]
async fn a_timestamped_data_directory_is_not_mistaken_for_a_log_mount() {
    a_data_directory_keeps_its_source(true).await;
}

/// 同一个数据目录，归档是升级之前建的（没有日志挂载记录）：只认创建时写进环境变量的那两个值，
/// 它一个都对不上。名字像日志目录不算证据，照样不换源。Codex 复现的就是这个形态。
#[tokio::test]
async fn a_timestamped_data_directory_in_a_legacy_archive_is_not_mistaken_for_a_log_mount_either() {
    a_data_directory_keeps_its_source(false).await;
}

/// 带数据的目录 `/mnt/project` 挂在日志根下面，日志挂载照常。`recorded` 说归档带不带日志挂载记录。
/// 断言数据挂载原样重放、登记，两条记录（换源记录、日志挂载记录）里都没有它
async fn a_data_directory_keeps_its_source(recorded: bool) {
    let mut restart = LoggedRestart::new();
    let project_directory = restart.host_root().join("customer-data-20251026080000");
    std::fs::create_dir_all(&project_directory).unwrap();
    restart.expose_bind_source(project_directory.to_str().unwrap());
    std::fs::write(project_directory.join("keep.txt"), b"project data").unwrap();
    let project = project_directory.to_string_lossy().into_owned();
    restart
        .template
        .volumes
        .push(host_path_volume("/mnt/project", &project));
    restart
        .archive
        .binds
        .push(format!("{project}:/mnt/project"));
    if !recorded {
        restart.archive.log_mounts = None;
    }
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

    let body = daemon.body(4);
    // 数据挂载原样重放：绑的还是原来那个目录
    let binds = body["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{project}:/mnt/project"))),
        "the data mount must keep its source: {binds:?}"
    );
    // 换源记录和日志挂载记录里都没有它
    for label in [RESTART_LOG_DIR, LOG_MOUNTS_LABEL] {
        let records = body["Labels"][label]
            .as_str()
            .and_then(|label| serde_json::from_str::<Value>(label).ok());
        assert!(
            records
                .as_ref()
                .and_then(|records| records.get("/mnt/project"))
                .is_none(),
            "a data mount is never recorded as a log mount: {label} {records:?}"
        );
    }
    assert!(
        project_directory.join("keep.txt").is_file(),
        "its data is untouched"
    );
    // 源登记到替代容器建出来
    let seen = seen_at_create.lock().unwrap().clone();
    assert!(
        seen.contains(&PathBuf::from(&project)),
        "the data mount is registered until the replacement exists: {seen:?}"
    );
    // 日志挂载照换
    let fresh = restart
        .host_root()
        .join(restart.bound_log_name(&daemon, 4, LOG_TARGET))
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &fresh)])
    );
}

/// 旧归档里挂载点带着 `container-logs` 的数据目录：挂载点怎么拼写也不是证据。创建时写环境变量
/// 点名的是 `/app/container-logs`，不是它。
#[tokio::test]
async fn a_data_mount_whose_target_mentions_container_logs_is_not_a_log_mount() {
    let mut restart = LoggedRestart::new();
    restart.archive.log_mounts = None;
    let backup_directory = restart.host_root().join("project");
    std::fs::create_dir_all(&backup_directory).unwrap();
    restart.expose_bind_source(backup_directory.to_str().unwrap());
    std::fs::write(backup_directory.join("keep.txt"), b"backup").unwrap();
    let backup = backup_directory.to_string_lossy().into_owned();
    restart
        .template
        .volumes
        .push(host_path_volume("/mnt/container-logs-backup", &backup));
    restart
        .archive
        .binds
        .push(format!("{backup}:/mnt/container-logs-backup"));
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    restart.restore(&daemon).await.unwrap();

    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{backup}:/mnt/container-logs-backup"))),
        "the data mount keeps its source: {binds:?}"
    );
    assert!(
        backup_directory.join("keep.txt").is_file(),
        "its data is untouched"
    );
    let records = daemon.body(4)["Labels"][RESTART_LOG_DIR]
        .as_str()
        .and_then(|label| serde_json::from_str::<Value>(label).ok());
    assert!(
        records
            .as_ref()
            .and_then(|records| records.get("/mnt/container-logs-backup"))
            .is_none(),
        "it is never swapped: {records:?}"
    );
}

/// 记录点名了这个挂载点，但源和记录对不上：记录之外有人动过这条 bind（或者把别的目录挂了过
/// 来）。记录说的那个才算日志挂载，现在这个不算，保留原 bind。
#[tokio::test]
async fn a_bind_whose_source_differs_from_the_record_is_not_a_log_mount() {
    let mut restart = LoggedRestart::new();
    // 记录说的是 `old_directory`；归档里实际绑的是另一个带数据的目录
    let replaced_directory = restart.host_root().join("project");
    std::fs::create_dir_all(&replaced_directory).unwrap();
    restart.expose_bind_source(replaced_directory.to_str().unwrap());
    std::fs::write(replaced_directory.join("keep.txt"), b"data").unwrap();
    let replaced = replaced_directory.to_string_lossy().into_owned();
    for volume in &mut restart.template.volumes {
        if volume.name == LOG_TARGET {
            volume.uid = replaced.clone();
        }
    }
    restart.archive.binds[0] = format!("{replaced}:{LOG_TARGET}:rw,z");
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    let replacement = restart.restore(&daemon).await.unwrap();

    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{replaced}:{LOG_TARGET}:rw,z"))),
        "the bind is replayed as archived: {binds:?}"
    );
    assert!(daemon.body(4)["Labels"][RESTART_LOG_DIR].is_null());
    assert!(
        replaced_directory.join("keep.txt").is_file(),
        "its data is untouched"
    );
    assert_eq!(replacement.expected_volumes, restart.template.volumes);
}

/// 来源不可信的 bind（不在记录里）落在日志根下、又没有挂载见证：改不了它，也不拒绝恢复——
/// 照原样重放，源登记到替代容器建出来。目录名像日志目录不算证据。
#[tokio::test]
async fn an_unrecorded_bind_into_the_log_root_is_replayed_and_registered() {
    let restart = LoggedRestart::new();
    let mut archive = restart.archive.clone();
    let stray = format!(
        "{}/customer-data-20251026080000",
        restart.host_root().display()
    );
    archive.binds.push(format!("{stray}:/mnt/project"));
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

    let replacement = restart.restore_archive(&daemon, archive).await.unwrap();

    daemon.assert_followed(5);
    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{stray}:/mnt/project"))),
        "the bind is replayed as archived: {binds:?}"
    );
    assert!(
        seen_at_create
            .lock()
            .unwrap()
            .contains(&PathBuf::from(&stray)),
        "its source is registered until the replacement exists"
    );
    // 记录里的那条日志挂载照换
    let fresh = restart
        .host_root()
        .join(restart.bound_log_name(&daemon, 4, LOG_TARGET))
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &fresh)])
    );
}

/// 有挂载见证、又不是日志挂载的一条 bind，源判不出在不在日志根里（经过一个指到 RCoder 挂载
/// 之外的符号链接，宿主机路径在本进程直接看得见）：照原样重放，源同样登记到替代容器建出来。
/// 旧容器一删，这个目录就不在任何容器的挂载里了，而清理器对它同样判不出来——登记着，清理器
/// 才会保留所有候选
#[tokio::test]
async fn an_unclassifiable_data_mount_with_a_witness_is_replayed_and_registered() {
    let mut restart = LoggedRestart::new();
    let outside = tempfile::tempdir().unwrap();
    let notes = sources::add_unconfirmable_data_mount(&mut restart, outside.path(), true);
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
    let mut seen = seen_at_create.lock().unwrap().clone();
    seen.sort();
    let mut expected = vec![
        PathBuf::from(new_log_directory(&restart, &name)),
        PathBuf::from(&notes),
    ];
    expected.sort();
    assert_eq!(
        seen, expected,
        "while the request is in flight, the unclassifiable source is claimed too"
    );
}
