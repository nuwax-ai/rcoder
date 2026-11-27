//! 重启协议：换源计划在动旧容器之前定下；运行时向引擎点名会换源的挂载；经公开恢复入口时，
//! 最终核验的必须是读到 label 的那一个替代容器。

use super::*;

/// 换源计划被拒绝时，原容器一根手指都不碰：能提前定的都定在动旧容器之前，连 Docker 都不问。
#[tokio::test]
async fn a_refused_plan_leaves_the_retained_original_in_place() {
    let restart = LoggedRestart::new();
    let mut archive = restart.archive.clone();
    archive.binds.push(format!(
        "{}/stray-20251026080000:/app/stray",
        restart.host_root().display()
    ));
    let daemon = scripted(vec![]).await;

    let result = restart.restore_archive(&daemon, archive).await;

    assert!(result.is_err(), "{result:?}");
    daemon.assert_no_request();
    assert!(std::path::Path::new(&restart.old_directory).is_dir());
}

/// 引擎重启后重新采集挂载时只放行运行时点名的这几条：源落在日志根下面的挂载，别的没有。
#[tokio::test]
async fn replaced_mounts_are_exactly_the_ones_bound_into_the_log_root() {
    let restart = LoggedRestart::with_audit_log();
    let daemon = scripted(vec![]).await;
    let log_runtime = restart.runtime(daemon.address);

    let replaced = log_runtime
        .restart_replaced_mounts(&restart.template)
        .await
        .unwrap();

    let mut replaced = replaced;
    replaced.sort();
    assert_eq!(replaced, [AUDIT_TARGET, LOG_TARGET]);
    daemon.assert_followed(0);

    // 没有日志挂载的 builder：一条都不放行
    let workspace = tempfile::tempdir().unwrap();
    let template = restart_template(workspace.path());
    let daemon = scripted(vec![]).await;
    assert!(
        runtime(daemon.address)
            .restart_replaced_mounts(&template)
            .await
            .unwrap()
            .is_empty()
    );
}

/// 一份落在磁盘上的重启归档，返回指向它的模板（卷的顺序和真实归档一样按挂载点排好）
fn archived(receipts: &std::path::Path, restart: &LoggedRestart) -> BuilderRestartTemplate {
    use sha2::{Digest as _, Sha256};
    let payload = serde_json::to_vec(&restart.archive).unwrap();
    let digest: String = Sha256::digest(&payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut template = restart.template.clone();
    template
        .volumes
        .sort_by(|a, b| (&a.name, &a.uid).cmp(&(&b.name, &b.uid)));
    template.archive = AppResourceIdentity {
        kind: AppResourceKind::File,
        name: "restart".into(),
        uid: digest,
        resource_version: None,
    };
    let directory = receipts
        .join(".app-operation-receipts")
        .join(&template.source.context.app_id);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("restart.json"), &payload).unwrap();
    template
}

/// 替代容器的 inspect 响应，带着重新采集时要求的身份 label
fn inspectable(
    template: &BuilderRestartTemplate,
    id: &str,
    extra_labels: Value,
    mounts: Value,
) -> Value {
    let context = &template.source.context;
    let mut labels: serde_json::Map<String, Value> = context
        .resource_metadata()
        .into_iter()
        .map(|(key, value)| (key, json!(value)))
        .collect();
    labels.insert(
        "service-type".into(),
        json!(ServiceType::UserappBuilder.to_string()),
    );
    labels.insert("identifier".into(), json!(context.app_id));
    for (key, value) in extra_labels.as_object().into_iter().flatten() {
        labels.insert(key.clone(), value.clone());
    }
    json!({
        "Id": id,
        "Name": format!("/{}", original(template).name),
        "Created": "2026-01-01T00:00:00Z",
        "Config": {"Labels": labels},
        "State": {"Status": "exited", "Running": false, "ExitCode": 0},
        "Mounts": mounts,
        "NetworkSettings": {"Networks": {}}
    })
}

fn resumed_labels(restart: &LoggedRestart, template: &BuilderRestartTemplate) -> Value {
    json!({
        RESTORE: template.archive.uid.clone(),
        RESTART_LOG_DIR: json!({
            LOG_TARGET: new_log_directory(restart, "rcoder-app-builder-265-20251026090000")
        })
        .to_string(),
    })
}

/// 经公开恢复入口：续做的替代容器就是重新采集到的那个，它实际绑定的也正是 label 记的目录。
#[tokio::test]
#[allow(unsafe_code)] // 测试内设置进程环境变量；nextest 每个用例独立进程
async fn the_public_restore_accepts_the_replacement_it_inspected() {
    let receipts = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RCODER_OPERATION_RECEIPT_ROOT", receipts.path());
    }
    let restart = LoggedRestart::new();
    let template = archived(receipts.path(), &restart);
    let fresh_log = new_log_directory(&restart, "rcoder-app-builder-265-20251026090000");
    let workspace = restart.workspace.path().to_string_lossy().into_owned();
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(&restart.template, resumed_labels(&restart, &template)),
        ),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            inspectable(
                &template,
                REPLACEMENT_UID,
                resumed_labels(&restart, &template),
                json!([
                    {"Type": "bind", "Source": fresh_log, "Destination": LOG_TARGET},
                    {"Type": "bind", "Source": workspace, "Destination": WORKSPACE_TARGET}
                ]),
            ),
        ),
        step(
            "GET",
            REPLACEMENT_UID,
            200,
            inspectable(
                &template,
                REPLACEMENT_UID,
                resumed_labels(&restart, &template),
                json!([
                    {"Type": "bind", "Source": fresh_log, "Destination": LOG_TARGET},
                    {"Type": "bind", "Source": workspace, "Destination": WORKSPACE_TARGET}
                ]),
            ),
        ),
    ])
    .await;
    let runtime = restart.runtime(daemon.address);

    let restored = runtime.restore_builder_template(&template).await.unwrap();

    daemon.assert_followed(4);
    assert_eq!(
        restored
            .workload
            .as_ref()
            .map(|resource| resource.uid.as_str()),
        Some(REPLACEMENT_UID)
    );
}

/// 经公开恢复入口的协议回归：最终核验的必须是读到 label 的那一个替代容器。
///
/// 第一次按名称查到的是 X（带着日志目录 label）；恢复之后重新采集，同名容器已经换成了 Y。
/// X 记的 label 对 Y 不作数，拒绝。
#[tokio::test]
#[allow(unsafe_code)] // 测试内设置进程环境变量；nextest 每个用例独立进程
async fn a_replacement_swapped_in_before_the_final_check_is_refused() {
    let receipts = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RCODER_OPERATION_RECEIPT_ROOT", receipts.path());
    }
    let restart = LoggedRestart::new();
    let template = archived(receipts.path(), &restart);
    let usurper = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(&restart.template, resumed_labels(&restart, &template)),
        ),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            inspectable(
                &template,
                usurper,
                resumed_labels(&restart, &template),
                json!([]),
            ),
        ),
    ])
    .await;
    let runtime = restart.runtime(daemon.address);

    let result = runtime.restore_builder_template(&template).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Restart target was replaced before it could be verified")),
        "{result:?}"
    );
    // 认下的是 X，重新采集到的是 Y：不会再去按 Y 的身份核对挂载
    daemon.assert_followed(3);
}

/// 工作区核对不过时不建日志目录。
#[tokio::test]
async fn no_log_directory_is_created_when_another_bind_source_is_missing() {
    let restart = LoggedRestart::new();
    std::fs::remove_dir_all(restart.workspace.path()).unwrap();
    let daemon = scripted(absent_original()).await;

    let result = restart.restore(&daemon).await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Restart bind source is missing")),
        "{result:?}"
    );
    daemon.assert_followed(2);
    assert!(!restart.visible_root().exists());
}
