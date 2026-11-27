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
    assert!(
        body["Labels"][LOG_MOUNTS_LABEL].is_null(),
        "nothing was created"
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
