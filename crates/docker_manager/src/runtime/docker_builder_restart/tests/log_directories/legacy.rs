//! 升级之前建的容器：归档里没有日志挂载记录，只认创建时写进环境变量的那两个值。

use super::*;

/// 升级之前建的容器：归档没有日志挂载记录。创建时写进环境变量的那两个值是它唯一的凭据——
/// 挂载点就是 `CONTAINER_LOGS_DIR`，照样换成新目录。
#[tokio::test]
async fn a_legacy_archive_is_recognised_by_the_log_env_it_was_created_with() {
    let restart = LoggedRestart::new();
    let mut archive = restart.archive.clone();
    archive.log_mounts = None;
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;

    let replacement = restart.restore_archive(&daemon, archive).await.unwrap();

    daemon.assert_followed(5);
    let name = restart.bound_log_name(&daemon, 4, LOG_TARGET);
    let new_directory = new_log_directory(&restart, &name);
    assert_ne!(new_directory, restart.old_directory);
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &new_directory)])
    );
    // 这次换出来的目录是 RCoder 新建的：替代容器从此有了记录
    assert_eq!(
        daemon.body(4)["Labels"][LOG_MOUNTS_LABEL]
            .as_str()
            .and_then(|label| serde_json::from_str::<Value>(label).ok()),
        Some(json!([{"target": LOG_TARGET, "source": new_directory}]))
    );
}

/// 旧归档里连环境变量都对不上（带变量的模板）：来源不可信，保留原 bind，源登记到替代容器建
/// 出来。只看源路径、或者只看目录名像不像日志目录，都可能把一份数据换成空目录。
#[tokio::test]
async fn a_legacy_archive_without_any_evidence_keeps_its_log_bind() {
    let restart = LoggedRestart::new();
    restart.expose_bind_source(&restart.old_directory);
    let mut archive = restart.archive.clone();
    archive.log_mounts = None;
    archive.env[0] = "CONTAINER_LOGS_DIR=/app/container-logs/{timestamp}".into();
    archive.env[1] = "CONTAINER_LOG_NAME={log_dir_name}".into();
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
    let body = daemon.body(4);
    let binds = body["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!(
            "{}:{LOG_TARGET}:rw,z",
            restart.old_directory
        ))),
        "the bind is replayed as archived: {binds:?}"
    );
    assert!(body["Labels"][RESTART_LOG_DIR].is_null());
    assert_eq!(
        body["Labels"][LOG_MOUNTS_LABEL].as_str(),
        Some("[]"),
        "nothing was swapped, and the replacement is recorded as having no log mounts of its own"
    );
    assert_eq!(replacement.expected_volumes, restart.template.volumes);
    assert!(
        seen_at_create
            .lock()
            .unwrap()
            .contains(&PathBuf::from(&restart.old_directory)),
        "the archived directory is registered until the replacement exists"
    );
}

/// 归档带着一份**空的**日志挂载记录：这个容器是新版建的，只是没有 RCoder 独占新建的日志挂载。
/// 环境变量哪怕指着一条挂在日志根里的 bind，也不退回去认——空记录就是为了和"升级之前建的、
/// 没有这份记录"区分开
#[tokio::test]
async fn an_explicitly_empty_record_disables_the_log_env_fallback() {
    let mut restart = LoggedRestart::new();
    restart.expose_bind_source(&restart.old_directory);
    restart.archive.log_mounts = Some(Vec::new());
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

    daemon.assert_followed(5);
    let body = daemon.body(4);
    let binds = body["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!(
            "{}:{LOG_TARGET}:rw,z",
            restart.old_directory
        ))),
        "the bind is kept: {binds:?}"
    );
    assert!(body["Labels"][RESTART_LOG_DIR].is_null());
    assert_eq!(body["Labels"][LOG_MOUNTS_LABEL].as_str(), Some("[]"));
    assert_eq!(replacement.expected_volumes, restart.template.volumes);
    assert!(
        seen_at_create
            .lock()
            .unwrap()
            .contains(&PathBuf::from(&restart.old_directory)),
        "the kept directory is registered until the replacement exists"
    );
}

/// 回归（Codex 第六轮 P1）：旧归档里，真正的日志挂载和环境变量都对得上；另外还有一条数据挂载，
/// 在别的父目录下，目录名却和日志目录一模一样。
///
/// 只凭"源的文件名等于 CONTAINER_LOG_NAME"就换源的话，这条数据挂载会被换成新建的空目录。两个
/// 环境变量要**一起**对上同一条 bind 才算：挂载点是 CONTAINER_LOGS_DIR，并且源的文件名是
/// CONTAINER_LOG_NAME。
#[tokio::test]
async fn a_same_named_data_directory_in_a_legacy_archive_is_not_mistaken_for_the_log_mount() {
    let mut restart = LoggedRestart::new();
    restart.archive.log_mounts = None;
    let data_directory = restart.host_root().join("project").join(&restart.old_name);
    std::fs::create_dir_all(&data_directory).unwrap();
    restart.expose_bind_source(data_directory.to_str().unwrap());
    std::fs::write(data_directory.join("keep.txt"), b"project data").unwrap();
    let data = data_directory.to_string_lossy().into_owned();
    restart
        .template
        .volumes
        .push(host_path_volume("/mnt/project", &data));
    restart.archive.binds.push(format!("{data}:/mnt/project"));
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

    daemon.assert_followed(5);
    let body = daemon.body(4);
    let binds = body["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{data}:/mnt/project"))),
        "the data mount keeps its source: {binds:?}"
    );
    assert!(
        data_directory.join("keep.txt").is_file(),
        "its data is untouched"
    );
    // 真正的日志挂载照换，换源记录里只有它
    let fresh = restart
        .host_root()
        .join(restart.bound_log_name(&daemon, 4, LOG_TARGET))
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        restart.bound_sources(&daemon, 4),
        json!({ LOG_TARGET: fresh }).as_object().unwrap().clone()
    );
    assert!(
        seen_at_create
            .lock()
            .unwrap()
            .contains(&PathBuf::from(&data)),
        "the data mount is registered until the replacement exists"
    );
    assert_eq!(
        replacement.expected_volumes,
        restart.expected_volumes(&[(LOG_TARGET, &fresh)])
    );
}

/// 旧归档里挂载点对得上 CONTAINER_LOGS_DIR，目录名却不是 CONTAINER_LOG_NAME（早期配置的日志目录
/// 不带时间戳）：两个值没有一起对上，证据不足，保留原 bind 并登记
#[tokio::test]
async fn a_legacy_log_mount_whose_directory_has_another_name_is_kept() {
    let mut restart = LoggedRestart::new();
    restart.archive.log_mounts = None;
    let old_style_directory = restart.host_root().join("project-logs");
    std::fs::create_dir_all(&old_style_directory).unwrap();
    restart.expose_bind_source(old_style_directory.to_str().unwrap());
    let old_style = old_style_directory.to_string_lossy().into_owned();
    for volume in &mut restart.template.volumes {
        if volume.name == LOG_TARGET {
            volume.uid = old_style.clone();
        }
    }
    restart.archive.binds[0] = format!("{old_style}:{LOG_TARGET}:rw,z");
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

    daemon.assert_followed(5);
    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{old_style}:{LOG_TARGET}:rw,z"))),
        "the bind is kept: {binds:?}"
    );
    assert!(daemon.body(4)["Labels"][RESTART_LOG_DIR].is_null());
    assert!(
        seen_at_create
            .lock()
            .unwrap()
            .contains(&PathBuf::from(&old_style)),
        "the kept directory is registered until the replacement exists"
    );
    assert_eq!(replacement.expected_volumes, restart.template.volumes);
}

/// 旧归档里没有挂载见证的 bind，源的文件名正好是 CONTAINER_LOG_NAME：没有挂载点去和
/// CONTAINER_LOGS_DIR 对，单凭名字不够。不换（本来也改不了它），也不拒绝恢复——照原样重放并登记
#[tokio::test]
async fn a_stray_bind_named_like_the_log_directory_in_a_legacy_archive_is_replayed() {
    let mut restart = LoggedRestart::new();
    restart.archive.log_mounts = None;
    let stray_directory = restart.host_root().join("project").join(&restart.old_name);
    std::fs::create_dir_all(&stray_directory).unwrap();
    let stray = stray_directory.to_string_lossy().into_owned();
    restart.archive.binds.push(format!("{stray}:/app/stray"));
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

    daemon.assert_followed(5);
    let binds = daemon.body(4)["HostConfig"]["Binds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        binds.contains(&json!(format!("{stray}:/app/stray"))),
        "the bind is replayed as archived: {binds:?}"
    );
    assert!(
        seen_at_create
            .lock()
            .unwrap()
            .contains(&PathBuf::from(&stray)),
        "its source is registered until the replacement exists"
    );
}
