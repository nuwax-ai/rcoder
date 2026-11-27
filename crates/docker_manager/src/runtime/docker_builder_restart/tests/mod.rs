//! builder 重启恢复：真实 Bollard 客户端对着假 daemon，不依赖时序。
//!
//! 假 daemon 按剧本逐步响应，并记下每个请求的正文。替代容器绑定了什么、带了哪些环境变量和
//! label，都在创建请求的正文里，可以确定性地断言，不用靠 sleep 碰运气。
//!
//! 这里有恢复所需的输入、用例，以及不涉及日志目录的几条用例；替代容器的日志目录在
//! [`log_directories`]。

use super::*;
use crate::runtime::fake_daemon::{
    ScriptedDaemon, Step, daemon_info, image_inspect, lost_reply, no_such_container, scripted,
    scripted_with_hook, stalled, step,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

mod log_directories;

const ORIGINAL_UID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REPLACEMENT_UID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// builder 工作区在容器内的挂载点；恢复时要求至少有一个这样的数据挂载
const WORKSPACE_TARGET: &str = "/home/user/userapp";
/// 日志目录在容器内的挂载点
const LOG_TARGET: &str = "/app/container-logs";

// ─── 假 daemon ──────────────────────────────────────────────────────────────
// 假 daemon 本身在 `crate::runtime::fake_daemon`，别的测试也用它。

/// 创建替代容器的剧本：原容器已不在，镜像就绪后创建
fn creation_script(create: Step) -> Vec<Step> {
    let mut script = absent_original();
    script.extend(creation_prelude(create));
    script
}

// ─── 恢复所需的输入 ─────────────────────────────────────────────────────────

fn docker_manager(address: std::net::SocketAddr) -> Arc<crate::DockerManager> {
    docker_manager_configured(address, crate::DockerManagerConfig::default())
}

/// 直接装配 `DockerManager`（不做容器自检）
fn docker_manager_configured(
    address: std::net::SocketAddr,
    config: crate::DockerManagerConfig,
) -> Arc<crate::DockerManager> {
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    Arc::new(crate::DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config,
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    })
}

/// 挂载表里没有日志根：没有 bind 落在可回收的日志域里。不做容器自检
fn runtime(address: std::net::SocketAddr) -> DockerRuntime {
    runtime_with_mounts(
        address,
        vec![(
            PathBuf::from("/app/workspace"),
            PathBuf::from("/restart-fence-workspace"),
        )],
    )
}

/// 用注入的挂载表构造运行时：RCoder 进程里的路径对应宿主机上的路径，不做容器自检
fn runtime_with_mounts(
    address: std::net::SocketAddr,
    mounts: Vec<(PathBuf, PathBuf)>,
) -> DockerRuntime {
    let resolver = crate::HostPathResolver::from_mount_table(mounts).unwrap();
    DockerRuntime::with_log_path_resolver(docker_manager(address), resolver)
}

fn control_target() -> BuilderControlTarget {
    let app_id = format!("restartfence{}", uuid::Uuid::new_v4().simple());
    BuilderControlTarget {
        resource_binding: Some(shared_types::UserAppResourceBinding {
            app_id: app_id.clone(),
            lifecycle_id: "lifecycle".into(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: ORIGINAL_UID.into(),
            adopted_by_operation: "adoption".into(),
        }),
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: format!("rcoder-app-builder-{app_id}"),
            uid: ORIGINAL_UID.into(),
            resource_version: None,
        }),
        context: UserAppExecutionContext {
            app_id,
            lifecycle_id: "lifecycle".into(),
            operation_id: "restart".into(),
            executor_id: "executor".into(),
            request_fingerprint: "a".repeat(64),
        },
        pod: None,
        restart_image: None,
        restart_runtime_workspace: None,
        builder_startup_mode: shared_types::BuilderStartupMode::default(),
    }
}

fn host_path_volume(target: &str, uid: &str) -> AppResourceIdentity {
    AppResourceIdentity {
        kind: AppResourceKind::HostPath,
        name: target.into(),
        uid: uid.into(),
        resource_version: None,
    }
}

fn template_for(volumes: Vec<AppResourceIdentity>) -> BuilderRestartTemplate {
    BuilderRestartTemplate {
        source: control_target(),
        volumes,
        archive: AppResourceIdentity {
            kind: AppResourceKind::File,
            name: "restart-archive.json".into(),
            uid: "archive-digest".into(),
            resource_version: None,
        },
    }
}

/// 一份重启模板：原容器是 `ORIGINAL_UID`，工作区挂在 `workspace`（宿主机路径）
fn restart_template(workspace: &std::path::Path) -> BuilderRestartTemplate {
    template_for(vec![host_path_volume(
        WORKSPACE_TARGET,
        &workspace.to_string_lossy(),
    )])
}

fn archive_for(template: &BuilderRestartTemplate, binds: Vec<String>, env: Vec<String>) -> Archive {
    Archive {
        source: template.source.clone(),
        image: "builder-image:1".into(),
        binds,
        network_mode: "bridge".into(),
        network_name: None,
        env,
        cmd: None,
        entrypoint: None,
        working_dir: None,
        hostname: None,
        domainname: None,
        security_opt: None,
        memory: None,
        nano_cpus: None,
        cpu_shares: None,
        cpuset_cpus: None,
        log_mounts: None,
    }
}

fn restart_archive(template: &BuilderRestartTemplate) -> Archive {
    let workspace = template.volumes[0].uid.clone();
    archive_for(
        template,
        vec![format!("{workspace}:{WORKSPACE_TARGET}")],
        Vec::new(),
    )
}

fn original(template: &BuilderRestartTemplate) -> &AppResourceIdentity {
    template.source.workload.as_ref().unwrap()
}

/// 旧版本会保留停止后的 builder 容器：已退出，挂载与模板里的卷一致
fn retained_original(template: &BuilderRestartTemplate) -> Value {
    json!({
        "Id": ORIGINAL_UID,
        "Name": format!("/{}", original(template).name),
        "Created": "2026-01-01T00:00:00Z",
        "Config": {"Labels": {}},
        "State": {"Status": "exited", "Running": false, "ExitCode": 0},
        "Mounts": template.volumes.iter().map(|volume| json!({
            "Type": "bind", "Source": volume.uid, "Destination": volume.name
        })).collect::<Vec<_>>(),
        "NetworkSettings": {"Networks": {}}
    })
}

/// 上一次中断前已经建好的替代容器：已退出，带着给定的 label
fn retained_replacement(template: &BuilderRestartTemplate, labels: Value) -> Value {
    json!({
        "Id": REPLACEMENT_UID,
        "Name": format!("/{}", original(template).name),
        "Created": "2026-01-01T00:00:00Z",
        "Config": {"Labels": labels},
        "State": {"Status": "exited", "Running": false, "ExitCode": 0},
        "Mounts": [],
        "NetworkSettings": {"Networks": {}}
    })
}

fn created() -> Value {
    json!({"Id": REPLACEMENT_UID, "Warnings": []})
}

/// 原容器已随停止自动删除时的前两步：查不到原容器，也没有中断遗留的替代容器
fn absent_original() -> Vec<Step> {
    vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step("GET", "rcoder-app-builder-", 404, no_such_container()),
    ]
}

/// 建替代容器之前的准备：确认镜像、读取 daemon 信息
fn creation_prelude(create: Step) -> Vec<Step> {
    vec![
        step("GET", "/images/", 200, image_inspect()),
        step("GET", "/info", 200, daemon_info()),
        create,
    ]
}

// ─── 用例：不涉及日志目录 ───────────────────────────────────────────────────

#[tokio::test]
async fn the_replacement_is_created_once_the_image_is_ready() {
    let workspace = tempfile::tempdir().unwrap();
    let template = restart_template(workspace.path());
    let archive = restart_archive(&template);
    let daemon = scripted(creation_script(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )))
    .await;
    let runtime = runtime(daemon.address);

    let replacement = runtime
        .replace_builder_container(&template, &archive, original(&template))
        .await
        .unwrap();

    daemon.assert_followed(5);
    assert_eq!(replacement.id, REPLACEMENT_UID);
    assert_eq!(replacement.expected_volumes, template.volumes);
    let body = daemon.body(4);
    assert_eq!(
        body["HostConfig"]["Binds"],
        json!([format!("{}:{WORKSPACE_TARGET}", workspace.path().display())]),
        "binds outside the reclaimable log root are replayed verbatim"
    );
    assert_eq!(body["Labels"][RESTORE], template.archive.uid);
    assert!(body["Labels"].get(RESTART_LOG_DIR).is_none());
}

/// 旧版本保留的停止容器先被移除，替代容器才创建。
#[tokio::test]
#[allow(unsafe_code)] // 测试内设置进程环境变量；nextest 每个用例独立进程
async fn a_retained_original_is_removed_before_the_replacement_is_created() {
    let receipts = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RCODER_OPERATION_RECEIPT_ROOT", receipts.path());
    }
    let workspace = tempfile::tempdir().unwrap();
    let template = restart_template(workspace.path());
    let archive = restart_archive(&template);
    super::super::docker_compute_receipt::save_stop(&template.source)
        .await
        .unwrap();
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 200, retained_original(&template)),
        step("GET", "/images/", 200, image_inspect()),
        step("DELETE", ORIGINAL_UID, 204, json!({})),
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step("GET", "rcoder-app-builder-", 404, no_such_container()),
        step("GET", "/images/", 200, image_inspect()),
        step("GET", "/info", 200, daemon_info()),
        step("POST", "/containers/create", 200, created()),
    ])
    .await;
    let runtime = runtime(daemon.address);

    let replacement = runtime
        .replace_builder_container(&template, &archive, original(&template))
        .await
        .unwrap();

    daemon.assert_followed(8);
    assert_eq!(replacement.id, REPLACEMENT_UID);
}

/// 工作区这类数据挂载的源目录不在了，恢复必须拒绝，而且不能走到创建那一步。
#[tokio::test]
async fn a_missing_bind_source_conflicts_before_anything_is_created() {
    let workspace = tempfile::tempdir().unwrap();
    let gone = workspace.path().join("workspace");
    let template = restart_template(&gone);
    let archive = restart_archive(&template);
    let daemon = scripted(absent_original()).await;
    let runtime = runtime(daemon.address);

    let result = runtime
        .replace_builder_container(&template, &archive, original(&template))
        .await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Restart bind source is missing")),
        "{result:?}"
    );
    daemon.assert_followed(2);
}

/// 工作区锚点：认宿主机路径里第一级 `userapp-workspace`，它之后的部分原样接到 RCoder 的工作
/// 区根下面；没有这一级就不接
#[test]
fn the_workspace_anchor_maps_what_follows_the_first_userapp_workspace_component() {
    let root = std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT);
    let anchor = |source: &str| beneath_workspace_root(std::path::Path::new(source));
    assert_eq!(
        anchor("/srv/nuwax/userapp-workspace/app-1/workspace"),
        Some(root.join("app-1/workspace"))
    );
    assert_eq!(
        anchor("/srv/userapp-workspace/other/userapp-workspace/x"),
        Some(root.join("other/userapp-workspace/x"))
    );
    assert_eq!(anchor("/srv/nuwax/data/app-1"), None);
}

/// 源目录不在了，旧容器还留着（已停止）：核验要在移除它**之前**。移除之后才发现，旧容器连同
/// 它的配置一起没了，现场只剩一个报错
#[tokio::test]
#[allow(unsafe_code)] // 测试内设置进程环境变量；nextest 每个用例独立进程
async fn a_missing_bind_source_keeps_the_stopped_original() {
    let receipts = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("RCODER_OPERATION_RECEIPT_ROOT", receipts.path());
    }
    let workspace = tempfile::tempdir().unwrap();
    let gone = workspace.path().join("workspace");
    let template = restart_template(&gone);
    let archive = restart_archive(&template);
    super::super::docker_compute_receipt::save_stop(&template.source)
        .await
        .unwrap();
    let daemon = scripted(vec![step(
        "GET",
        ORIGINAL_UID,
        200,
        retained_original(&template),
    )])
    .await;
    let runtime = runtime(daemon.address);

    let result = runtime
        .replace_builder_container(&template, &archive, original(&template))
        .await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Restart bind source is missing")),
        "{result:?}"
    );
    // 只查了原容器：没有移除它，也没有往下走
    daemon.assert_followed(1);
}

/// 归档把原容器 label 上的日志挂载记录带下来。没有这个 label（升级之前建的容器）、或者它读不出来，
/// 都按没有处理：换源只认记录，没有记录一条都不换
#[test]
fn the_archive_carries_the_log_mount_record_of_the_original() {
    let records = vec![LogMountRecord {
        target: LOG_TARGET.into(),
        source: "/srv/logs/container/rcoder-app-builder-265-20261029035324".into(),
    }];
    let labelled = HashMap::from([(
        LOG_MOUNTS_LABEL.to_owned(),
        encode_log_mounts(&records),
    )]);
    assert_eq!(archived_log_mounts(Some(&labelled)), Some(records));

    let other_labels = HashMap::from([("service-type".to_owned(), "userapp-builder".to_owned())]);
    assert_eq!(archived_log_mounts(Some(&other_labels)), None);
    assert_eq!(archived_log_mounts(None), None);

    let broken = HashMap::from([(LOG_MOUNTS_LABEL.to_owned(), "not a record".to_owned())]);
    assert_eq!(archived_log_mounts(Some(&broken)), None);
}
