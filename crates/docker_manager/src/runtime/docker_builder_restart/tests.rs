//! builder 重启恢复与挂载围栏的交错：真实 Bollard 客户端对着假 daemon，不依赖时序。
//!
//! 假 daemon 按剧本逐步响应。带探针的请求到达时，它当场尝试获取日志清理器的写锁并记录
//! 是否成功：围栏的读锁被持有时写锁必然拿不到。于是"移除旧容器 → 核对 bind 源 → 创建"
//! 这一段有没有被围栏覆盖，可以被确定性地断言，而不是靠 sleep 碰运气。

use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const ORIGINAL_UID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REPLACEMENT_UID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// builder 工作区在容器内的挂载点；恢复时要求至少有一个这样的数据挂载
const WORKSPACE_TARGET: &str = "/home/user/userapp";

// ─── 假 daemon ──────────────────────────────────────────────────────────────

/// 一步预设响应。`probe` 有值时，这个请求到达的瞬间尝试获取清理器的写锁，
/// 以该标签记录是否成功。
struct Step {
    method: &'static str,
    contains: String,
    status: u16,
    body: Value,
    probe: Option<&'static str>,
}

fn step(method: &'static str, contains: impl Into<String>, status: u16, body: Value) -> Step {
    Step {
        method,
        contains: contains.into(),
        status,
        body,
        probe: None,
    }
}

fn no_such_container() -> Value {
    json!({"message": "No such container"})
}

fn image_inspect() -> Value {
    serde_json::to_value(bollard::models::ImageInspect::default()).unwrap()
}

fn daemon_info() -> Value {
    let info = bollard::models::SystemInfo {
        id: Some("restart-fence-daemon".into()),
        ..Default::default()
    };
    serde_json::to_value(info).unwrap()
}

struct ScriptedDaemon {
    address: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    probes: Arc<Mutex<Vec<(&'static str, bool)>>>,
    mismatches: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl ScriptedDaemon {
    /// 请求必须逐步命中剧本；任何偏离都记录下来
    fn assert_followed(&self, expected_requests: usize) {
        let mismatches = self.mismatches.lock().unwrap();
        assert!(mismatches.is_empty(), "unexpected requests: {mismatches:?}");
        assert_eq!(
            self.requests.lock().unwrap().len(),
            expected_requests,
            "every scripted request must have been made"
        );
    }

    /// 各探针处是否成功拿到了清理器的写锁
    fn cleanup_acquired_at(&self) -> Vec<(&'static str, bool)> {
        self.probes.lock().unwrap().clone()
    }
}

async fn scripted(steps: Vec<Step>) -> ScriptedDaemon {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut script = std::collections::VecDeque::new();
    script.extend(steps);
    let script = Arc::new(Mutex::new(script));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let probes = Arc::new(Mutex::new(Vec::new()));
    let mismatches = Arc::new(Mutex::new(Vec::new()));
    let (recorded, probe_log, failed) = (
        Arc::clone(&requests),
        Arc::clone(&probes),
        Arc::clone(&mismatches),
    );
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            // 先读完请求头和请求体再应答：客户端可能还在写请求体，过早关连接会让它读不到应答
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).await.is_ok_and(|n| n == 1)
            {
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let content_length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0_u8; content_length];
            if stream.read_exact(&mut body).await.is_err() {
                continue;
            }

            let request = head.lines().next().unwrap().to_owned();
            let expected = script.lock().unwrap().pop_front();
            let matched = expected.as_ref().is_some_and(|step| {
                request.starts_with(step.method) && request.contains(&step.contains)
            });
            if !matched {
                failed.lock().unwrap().push(format!(
                    "{request} (next step: {:?})",
                    expected
                        .as_ref()
                        .map(|step| (step.method, step.contains.clone()))
                ));
            }
            let step = expected.filter(|_| matched);
            if let Some(label) = step.as_ref().and_then(|step| step.probe) {
                probe_log
                    .lock()
                    .unwrap()
                    .push((label, crate::try_begin_log_mount_cleanup().is_some()));
            }
            recorded.lock().unwrap().push(request);
            let body = step.as_ref().map_or_else(
                || no_such_container().to_string(),
                |step| step.body.to_string(),
            );
            let status = step.as_ref().map_or(500, |step| step.status);
            let response = format!(
                "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _written = stream.write_all(response.as_bytes()).await;
        }
    });
    ScriptedDaemon {
        address,
        requests,
        probes,
        mismatches,
        server,
    }
}

// ─── 恢复所需的输入 ─────────────────────────────────────────────────────────

fn runtime(address: std::net::SocketAddr) -> DockerRuntime {
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    DockerRuntime::new(Arc::new(crate::DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: crate::DockerManagerConfig::default(),
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    }))
}

/// 一份重启模板：原容器是 `ORIGINAL_UID`，工作区挂在 `workspace`（宿主机路径）
fn restart_template(workspace: &std::path::Path) -> BuilderRestartTemplate {
    let context = UserAppExecutionContext {
        app_id: format!("restartfence{}", uuid::Uuid::new_v4().simple()),
        lifecycle_id: "lifecycle".into(),
        operation_id: "restart".into(),
        executor_id: "executor".into(),
        request_fingerprint: "a".repeat(64),
    };
    let source = BuilderControlTarget {
        resource_binding: Some(shared_types::UserAppResourceBinding {
            app_id: context.app_id.clone(),
            lifecycle_id: context.lifecycle_id.clone(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: ORIGINAL_UID.into(),
            adopted_by_operation: "adoption".into(),
        }),
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: format!("rcoder-app-builder-{}", context.app_id),
            uid: ORIGINAL_UID.into(),
            resource_version: None,
        }),
        context,
        pod: None,
        restart_image: None,
        restart_runtime_workspace: None,
        builder_startup_mode: shared_types::BuilderStartupMode::default(),
    };
    BuilderRestartTemplate {
        volumes: vec![AppResourceIdentity {
            kind: AppResourceKind::HostPath,
            name: WORKSPACE_TARGET.into(),
            uid: workspace.to_string_lossy().into_owned(),
            resource_version: None,
        }],
        source,
        archive: AppResourceIdentity {
            kind: AppResourceKind::File,
            name: "restart-archive.json".into(),
            uid: "archive-digest".into(),
            resource_version: None,
        },
    }
}

fn restart_archive(template: &BuilderRestartTemplate) -> Archive {
    let workspace = template.volumes[0].uid.clone();
    Archive {
        source: template.source.clone(),
        image: "builder-image:1".into(),
        binds: vec![format!("{workspace}:{WORKSPACE_TARGET}")],
        network_mode: "bridge".into(),
        network_name: None,
        env: Vec::new(),
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
    }
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

// ─── 用例 ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_fence_is_held_from_the_bind_check_until_the_replacement_is_created() {
    let workspace = tempfile::tempdir().unwrap();
    let template = restart_template(workspace.path());
    let archive = restart_archive(&template);
    let mut script = absent_original();
    script.extend([
        step("GET", "/images/", 200, image_inspect()),
        Step {
            probe: Some("after the bind check"),
            ..step("GET", "/info", 200, daemon_info())
        },
        Step {
            probe: Some("create"),
            ..step("POST", "/containers/create", 200, created())
        },
    ]);
    let daemon = scripted(script).await;
    let runtime = runtime(daemon.address);

    runtime
        .replace_builder_container(&template, &archive, original(&template))
        .await
        .unwrap();

    daemon.assert_followed(5);
    assert_eq!(
        daemon.cleanup_acquired_at(),
        [("after the bind check", false), ("create", false)],
        "cleanup must not interleave between the bind check and the create"
    );
    assert!(
        crate::try_begin_log_mount_cleanup().is_some(),
        "the fence must be released once the replacement exists"
    );
}

/// 旧版本保留的停止容器也在围栏内移除：它的挂载消失到替代容器登记之间不能有窗口。
#[tokio::test]
#[allow(unsafe_code)] // 测试内设置进程环境变量；nextest 每个用例独立进程
async fn a_retained_original_is_removed_inside_the_fence() {
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
        Step {
            probe: Some("before the removal"),
            ..step("GET", ORIGINAL_UID, 200, retained_original(&template))
        },
        step("GET", "/images/", 200, image_inspect()),
        Step {
            probe: Some("remove the original"),
            ..step("DELETE", ORIGINAL_UID, 204, json!({}))
        },
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step("GET", "rcoder-app-builder-", 404, no_such_container()),
        step("GET", "/images/", 200, image_inspect()),
        step("GET", "/info", 200, daemon_info()),
        Step {
            probe: Some("create"),
            ..step("POST", "/containers/create", 200, created())
        },
    ])
    .await;
    let runtime = runtime(daemon.address);

    runtime
        .replace_builder_container(&template, &archive, original(&template))
        .await
        .unwrap();

    daemon.assert_followed(8);
    assert_eq!(
        daemon.cleanup_acquired_at(),
        [
            ("before the removal", false),
            ("remove the original", false),
            ("create", false)
        ],
        "the fence must already be held before the original is touched"
    );
    assert!(crate::try_begin_log_mount_cleanup().is_some());
}

/// 工作区这类数据挂载的源目录不在了，恢复必须拒绝，而且不能走到创建那一步。
/// 日志清理器先回收了归档里的日志目录时也是这个结果，需要人工处理。
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
    assert!(
        crate::try_begin_log_mount_cleanup().is_some(),
        "an early return must still release the fence"
    );
}

/// 创建方持有围栏时清理拿不到写锁；创建方放开后清理恢复。
#[tokio::test]
async fn a_creator_holding_the_fence_excludes_cleanup_until_it_releases() {
    let reservation = crate::LOG_MOUNT_GATE.read().await;

    assert!(
        crate::try_begin_log_mount_cleanup().is_none(),
        "cleanup must not interleave with a creator holding the fence"
    );

    drop(reservation);

    assert!(
        crate::try_begin_log_mount_cleanup().is_some(),
        "cleanup must resume once no creator holds the fence"
    );
}
