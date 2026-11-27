//! 日志挂载记录怎么从原容器传到归档：采集归档时读原容器 label 上的 `rcoder.io/log-mounts`。

use super::replacement::inspectable;
use super::*;

/// 采集归档时把原容器 label 上的日志挂载记录带下来，和 bind、环境变量一起存进归档文件。重启
/// 这个 builder 时换源只认这份记录，它是原容器创建那一刻写下的
#[tokio::test]
#[allow(unsafe_code)] // 测试内设置进程环境变量；nextest 每个用例独立进程
async fn the_archive_carries_the_log_mount_record_from_the_original_container() {
    let receipts = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RCODER_OPERATION_RECEIPT_ROOT", receipts.path());
    }
    let restart = LoggedRestart::new();
    let template = &restart.template;
    let records = [LogMountRecord {
        target: LOG_TARGET.to_owned(),
        source: restart.old_directory.clone(),
    }];
    let label = encode_log_mounts(&records);
    let workspace = restart.workspace.path().to_string_lossy().into_owned();
    let mut inspect = inspectable(
        template,
        ORIGINAL_UID,
        json!({ LOG_MOUNTS_LABEL: label.clone() }),
        json!([
            {
                "Type": "bind",
                "Source": restart.old_directory,
                "Destination": LOG_TARGET
            },
            {"Type": "bind", "Source": workspace, "Destination": WORKSPACE_TARGET},
        ]),
    );
    inspect["Config"]["Image"] = json!("builder-image:1");
    inspect["Config"]["Env"] = json!(&restart.archive.env);
    inspect["HostConfig"] = json!({"Binds": &restart.archive.binds});
    // 第一次按容器 ID 读原容器（采集归档），第二次按名字读（采集后的漂移核对）
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 200, inspect.clone()),
        step("GET", "rcoder-app-builder-", 200, inspect.clone()),
    ])
    .await;
    let runtime = restart.runtime(daemon.address);

    let archived = runtime
        .archive_builder_template(&template.source)
        .await
        .unwrap()
        .unwrap();

    daemon.assert_followed(2);
    let payload = std::fs::read(
        receipts
            .path()
            .join(".app-operation-receipts")
            .join(&template.source.context.app_id)
            .join(format!("{}.json", archived.archive.name)),
    )
    .unwrap();
    let archive: Archive = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        archive.log_mounts.as_deref(),
        Some(records.as_slice()),
        "the record on the original container reaches the archive"
    );
    assert_eq!(archive.binds, restart.archive.binds);
}

/// 回归（Codex 第六轮）：原容器的记录是**空的**（这个版本建的，只是没有 RCoder 独占新建的日志挂载）。
/// 采集归档时空记录要原样存下来，不能当成"没有记录"——那样重启它会退回去看环境变量，把挂在
/// 日志根里的 bind 当成日志挂载换掉。
#[tokio::test]
#[allow(unsafe_code)] // 测试内设置进程环境变量；nextest 每个用例独立进程
async fn an_empty_record_survives_the_capture_and_still_disables_the_log_env_fallback() {
    let receipts = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RCODER_OPERATION_RECEIPT_ROOT", receipts.path());
    }
    let restart = LoggedRestart::new();
    restart.expose_bind_source(&restart.old_directory);
    let template = &restart.template;
    let workspace = restart.workspace.path().to_string_lossy().into_owned();
    let mut inspect = inspectable(
        template,
        ORIGINAL_UID,
        json!({ LOG_MOUNTS_LABEL: "[]" }),
        json!([
            {
                "Type": "bind",
                "Source": restart.old_directory,
                "Destination": LOG_TARGET
            },
            {"Type": "bind", "Source": workspace, "Destination": WORKSPACE_TARGET},
        ]),
    );
    inspect["Config"]["Image"] = json!("builder-image:1");
    inspect["Config"]["Env"] = json!(&restart.archive.env);
    inspect["HostConfig"] = json!({"Binds": &restart.archive.binds});
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 200, inspect.clone()),
        step("GET", "rcoder-app-builder-", 200, inspect.clone()),
    ])
    .await;
    let runtime = restart.runtime(daemon.address);

    let archived = runtime
        .archive_builder_template(&template.source)
        .await
        .unwrap()
        .unwrap();

    daemon.assert_followed(2);
    let payload = std::fs::read(
        receipts
            .path()
            .join(".app-operation-receipts")
            .join(&template.source.context.app_id)
            .join(format!("{}.json", archived.archive.name)),
    )
    .unwrap();
    let archive: Archive = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        archive.log_mounts.as_deref(),
        Some(&[][..]),
        "an empty record is a record, not a missing one"
    );

    // 环境变量明明指着这条挂在日志根里的 bind：有记录（空的）就不看环境变量，原样重放
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;
    restart.restore_archive(&daemon, archive).await.unwrap();
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
}
