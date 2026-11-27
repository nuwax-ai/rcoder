//! 续做 builder 重启时的挂载见证：默认沿用快照里的；调用方核实过换源之后，可以给出替代
//! 容器实际有的挂载作为新的见证。
use super::*;

/// 一个停在 Stopped 的 builder 重启：原容器是 `oldphysical`，见证里有一条工作区、一条日志
/// 目录。返回（记录，接替的 builder，快照里的见证）
async fn stopped_restart(
    store: &ToastyUserAppStore,
) -> (
    ComputeControlRecord,
    BuilderControlTarget,
    serde_json::Value,
) {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let app = store
        .ensure_identity(&format!("witness{id}"))
        .await
        .unwrap();
    let pending = store
        .admit_compute_control(&request(
            &app,
            &format!("restart{id}"),
            ComputeControlAction::Restart,
        ))
        .await
        .unwrap();
    let identity = compute_identity(&pending);
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let mut record = store
        .bind_compute_lease(&identity, &compute_receipt())
        .await
        .unwrap();
    let context = record.execution_context().unwrap();
    let resource = AppResourceIdentity {
        kind: AppResourceKind::Container,
        name: "workload".into(),
        uid: "oldphysical".into(),
        resource_version: Some("1".into()),
    };
    let witness = serde_json::json!([
        {"kind": "HostPath", "name": "/home/user/userapp", "uid": "/srv/workspace", "resource_version": null},
        {"kind": "HostPath", "name": "/app/container-logs", "uid": "/srv/logs/container/runner-old", "resource_version": null}
    ]);
    let builder = BuilderControlTarget {
        builder_startup_mode: Default::default(),
        context,
        resource_binding: None,
        workload: Some(resource.clone()),
        pod: None,
        restart_image: None,
        restart_runtime_workspace: None,
    };
    let mut checkpoint = serde_json::to_value(&builder).unwrap();
    checkpoint["builder_volumes"] = witness.clone();
    checkpoint["builder_restart_template"] = serde_json::to_value(BuilderRestartTemplate {
        source: builder.clone(),
        archive: AppResourceIdentity {
            kind: AppResourceKind::File,
            name: "archive".into(),
            uid: "archiveuid".into(),
            resource_version: None,
        },
        volumes: serde_json::from_value(witness.clone()).unwrap(),
    })
    .unwrap();
    for stage in [ComputeControlStage::Stopping, ComputeControlStage::Stopped] {
        let mut progress = compute_progress(&record, stage);
        progress.checkpoint = checkpoint.clone();
        record = store.advance_compute_control(&progress).await.unwrap();
    }
    let mut replacement = builder;
    replacement.workload.as_mut().unwrap().uid = "newphysical".into();
    (record, replacement, witness)
}

#[tokio::test]
async fn resumed_restart_keeps_the_recorded_mount_witness_unless_replacement_mounts_are_given() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("restart-witness.db"))
        .await
        .unwrap();

    // 不给出替代容器的挂载：见证原样沿用
    let (record, replacement, witness) = stopped_restart(&store).await;
    let claimed = store
        .resume_builder_restart_start(&record, &replacement)
        .await
        .unwrap();
    assert_eq!(claimed.checkpoint["workload"]["uid"], "newphysical");
    assert_eq!(claimed.checkpoint["builder_volumes"], witness);

    // 给出替代容器实际有的挂载（日志目录换了新的）：以它为准，别的字段照旧沿用
    let (record, replacement, witness) = stopped_restart(&store).await;
    let mut actual = witness.clone();
    actual[1]["uid"] = serde_json::json!("/srv/logs/container/runner-new");
    let claimed = store
        .resume_builder_restart_start_with_volumes(&record, &replacement, &actual)
        .await
        .unwrap();
    assert_eq!(claimed.checkpoint["workload"]["uid"], "newphysical");
    assert_eq!(
        claimed.checkpoint["builder_volumes"], actual,
        "the witness must describe the mounts the replacement actually has"
    );
    assert_eq!(
        claimed.checkpoint["builder_restart_template"],
        record.checkpoint["builder_restart_template"],
        "the archive reference is still carried over"
    );
    // 快照本身没被改动
    let current = store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.checkpoint["builder_volumes"], actual);

    store.shutdown().await.unwrap();
}
