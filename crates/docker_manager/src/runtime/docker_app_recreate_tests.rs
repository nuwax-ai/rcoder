//! prod 应用换镜像重建经假 daemon：重建的挂载是从旧容器的 `HostConfig` 原样抄的，也要过挂载
//! 守卫。数据目录落在日志根里就拒绝、旧容器一根手指都不碰；否则这些 bind 源从删旧容器之前
//! 一直登记到新容器建出来——中间这一段它们不在任何容器的挂载里。

use serde_json::{Value, json};
use shared_types::{
    AppResourceIdentity, AppResourceKind, ServiceType, UserAppExecutionContext,
    UserAppMutationTarget,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::docker_runtime::{DockerRuntime, app_deployment_name};
use super::fake_daemon::{
    ScriptedDaemon, Step, image_inspect, no_such_container, scripted_with_hook, step,
};
use crate::log_directory::pending_bind_sources;

const OLD_UID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW_UID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const APP_ID: &str = "recreateapp";

fn target() -> UserAppMutationTarget {
    UserAppMutationTarget {
        context: UserAppExecutionContext {
            app_id: APP_ID.into(),
            lifecycle_id: "lifecycle".into(),
            operation_id: "restart".into(),
            executor_id: "executor".into(),
            request_fingerprint: "a".repeat(64),
        },
        resource: AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: app_deployment_name(APP_ID),
            uid: OLD_UID.into(),
            resource_version: None,
        },
    }
}

/// 一个运行时：日志根是 `reclaim_root`，挂载表只有一条（`covered` 挂在它自己的路径上）
fn runtime(daemon: &ScriptedDaemon, reclaim_root: PathBuf, covered: &Path) -> DockerRuntime {
    let (actor, containers) = crate::ContainerStateActor::new();
    tokio::spawn(actor.run());
    let manager = Arc::new(crate::DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{}", daemon.address),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: crate::DockerManagerConfig {
            log_reclaim_root: reclaim_root,
            ..Default::default()
        },
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    });
    let resolver = crate::HostPathResolver::from_mount_table(vec![(
        covered.to_path_buf(),
        covered.to_path_buf(),
    )])
    .unwrap();
    DockerRuntime::with_log_path_resolver(manager, resolver)
}

/// 旧容器的 inspect：带着应用身份 label，`mounts` 是它的 `HostConfig.Mounts`
fn live(mounts: Value) -> Value {
    json!({
        "Id": OLD_UID,
        "Name": format!("/{}", app_deployment_name(APP_ID)),
        "Created": "2026-01-01T00:00:00Z",
        "Config": {
            "Labels": {
                shared_types::USERAPP_DOCKER_APP_ID_LABEL: APP_ID,
                "service-type": ServiceType::Userapp.to_string(),
                "managed-by": "rcoder-app-manager",
            },
            "Env": [],
            "Cmd": [],
        },
        "HostConfig": {"NetworkMode": "bridge", "Mounts": mounts},
        "State": {"Status": "running", "Running": true},
        "NetworkSettings": {"Networks": {}}
    })
}

/// 新容器的 inspect：有了地址
fn running() -> Value {
    json!({
        "Id": NEW_UID,
        "Created": "2026-01-01T00:00:00Z",
        "State": {"Status": "running", "Running": true},
        "NetworkSettings": {"Networks": {"bridge": {"IPAddress": "172.17.0.9"}}}
    })
}

/// 重建的剧本：查旧容器、确认镜像、删旧容器、确认它没了、建新容器、启动、读地址
fn script(mounts: Value) -> Vec<Step> {
    vec![
        step("GET", OLD_UID, 200, live(mounts)),
        step("GET", "/images/", 200, image_inspect()),
        step("DELETE", OLD_UID, 204, json!({})),
        step("GET", app_deployment_name(APP_ID), 404, no_such_container()),
        step(
            "POST",
            "/containers/create",
            201,
            json!({"Id": NEW_UID, "Warnings": []}),
        ),
        step("POST", "/start", 204, json!({})),
        step("GET", NEW_UID, 200, running()),
    ]
}

fn prod_mounts(covered: &Path) -> Vec<(PathBuf, Value)> {
    ["a1", "data/a1"]
        .into_iter()
        .map(|relative| {
            let source = covered.join("prod").join(relative);
            std::fs::create_dir_all(&source).unwrap();
            let mount = json!({
                "Type": "bind",
                "Source": source.to_string_lossy(),
                "Target": format!("/home/user/{}", relative.rsplit('/').next_back().unwrap()),
            });
            (source, mount)
        })
        .collect()
}

/// 守卫在删旧容器、建新容器这两个时刻登记着的 bind 源
async fn recorded_at_delete_and_create(
    covered: &Path,
    reclaim_root: PathBuf,
) -> (
    ScriptedDaemon,
    Vec<Vec<PathBuf>>,
    container_runtime_api::ContainerRuntimeResult<()>,
) {
    let mounts = prod_mounts(covered);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let daemon = scripted_with_hook(
        script(json!(
            mounts
                .iter()
                .map(|(_, mount)| mount.clone())
                .collect::<Vec<_>>()
        )),
        Arc::new(move |request: &str| {
            let deleting = request.starts_with("DELETE");
            let creating = request.starts_with("POST") && request.contains("/containers/create");
            if deleting || creating {
                recorder.lock().unwrap().push(pending_bind_sources());
            }
        }),
    )
    .await;
    let runtime = runtime(&daemon, reclaim_root, covered);
    let result = runtime
        .recreate_captured_target_with_image(&target(), "registry.test/app-runtime:0.2.0")
        .await;
    (daemon, seen.lock().unwrap().clone(), result.map(|_| ()))
}

/// 数据目录不在日志根里：重建照常。从删旧容器之前到新容器建出来，数据目录的 bind 源一直
/// 登记着；新容器建出来之后由 Docker 清单接管。
#[tokio::test]
async fn a_recreated_application_keeps_its_data_directories_registered_across_the_swap() {
    let covered = tempfile::tempdir().unwrap();
    let sources: Vec<PathBuf> = prod_mounts(covered.path())
        .into_iter()
        .map(|(source, _)| crate::normalize_path(&source))
        .collect();

    let (daemon, seen, result) =
        recorded_at_delete_and_create(covered.path(), covered.path().join("container")).await;

    result.unwrap_or_else(|error| panic!("the rebuild must converge: {error:#}"));
    daemon.assert_followed(7);
    assert_eq!(
        seen.len(),
        2,
        "one observation at the delete, one at the create"
    );
    let mut expected = sources.clone();
    expected.sort();
    for observation in &seen {
        let mut observed = observation.clone();
        observed.sort();
        assert_eq!(
            observed, expected,
            "the data directories are nobody's mount between the delete and the create"
        );
    }
    assert!(
        pending_bind_sources().is_empty(),
        "once the replacement is created, the Docker inventory alone protects them"
    );
}

/// 数据目录落在日志根里（日志根配得和 prod 目录树重叠）：重建拒绝，旧容器一根手指都不碰，
/// 也没有任何登记留下。
#[tokio::test]
async fn a_recreated_application_cannot_put_its_data_under_the_log_root() {
    let covered = tempfile::tempdir().unwrap();

    let (daemon, seen, result) =
        recorded_at_delete_and_create(covered.path(), covered.path().to_path_buf()).await;

    let error = result.unwrap_err();
    assert!(
        format!("{error:#}").contains("which lies under the reclaimable log root"),
        "{error:#}"
    );
    // 只查了旧容器和镜像；删旧容器的请求没有发出去
    let requests = daemon.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        !requests.iter().any(|request| request.starts_with("DELETE")),
        "the original container must be left alone: {requests:?}"
    );
    assert!(seen.is_empty());
    assert!(pending_bind_sources().is_empty());
}
