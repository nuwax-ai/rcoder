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
