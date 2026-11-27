//! builder 重启恢复：真实 Bollard 客户端对着假 daemon，不依赖时序。
//!
//! 假 daemon 按剧本逐步响应，并记下每个请求的正文。替代容器绑定了什么、带了哪些环境变量和
//! label，都在创建请求的正文里，可以确定性地断言，不用靠 sleep 碰运气。
//!
//! 这里有假 daemon、恢复所需的输入和用例，以及不涉及日志目录的几条用例；替代容器的日志
//! 目录在 [`log_directories`]。

use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

mod log_directories;

const ORIGINAL_UID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REPLACEMENT_UID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// builder 工作区在容器内的挂载点；恢复时要求至少有一个这样的数据挂载
const WORKSPACE_TARGET: &str = "/home/user/userapp";
/// 日志目录在容器内的挂载点
const LOG_TARGET: &str = "/app/container-logs";

// ─── 假 daemon ──────────────────────────────────────────────────────────────

/// 一步预设响应
struct Step {
    method: &'static str,
    contains: String,
    status: u16,
    body: Value,
    /// 不应答，直接断开连接：模拟应答丢失
    hang_up: bool,
    /// 收下请求之后一直不应答：模拟 daemon 迟迟不回
    stall: bool,
}

fn step(method: &'static str, contains: impl Into<String>, status: u16, body: Value) -> Step {
    Step {
        method,
        contains: contains.into(),
        status,
        body,
        hang_up: false,
        stall: false,
    }
}

/// 应答丢失
fn lost_reply(method: &'static str, contains: impl Into<String>) -> Step {
    Step {
        method,
        contains: contains.into(),
        status: 0,
        body: Value::Null,
        hang_up: true,
        stall: false,
    }
}

/// 收下请求之后一直不应答
fn stalled(method: &'static str, contains: impl Into<String>) -> Step {
    Step {
        method,
        contains: contains.into(),
        status: 0,
        body: Value::Null,
        hang_up: false,
        stall: true,
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

type RequestHook = Arc<dyn Fn(&str) + Send + Sync>;

/// 创建替代容器的剧本：原容器已不在，镜像就绪后创建
fn creation_script(create: Step) -> Vec<Step> {
    let mut script = absent_original();
    script.extend(creation_prelude(create));
    script
}

struct ScriptedDaemon {
    address: std::net::SocketAddr,
    /// 请求行，按到达顺序
    requests: Arc<Mutex<Vec<String>>>,
    /// 请求正文，与 `requests` 一一对应
    bodies: Arc<Mutex<Vec<String>>>,
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

    /// 一个请求都没到过
    fn assert_no_request(&self) {
        assert!(
            self.requests.lock().unwrap().is_empty(),
            "no request must reach Docker: {:?}",
            self.requests.lock().unwrap()
        );
    }

    /// 第 `index` 个请求的正文
    fn body(&self, index: usize) -> Value {
        let bodies = self.bodies.lock().unwrap();
        serde_json::from_str(&bodies[index])
            .unwrap_or_else(|error| panic!("request {index} has no JSON body: {error}"))
    }
}

async fn scripted(steps: Vec<Step>) -> ScriptedDaemon {
    scripted_with_hook(steps, Arc::new(|_: &str| {})).await
}

/// 假 daemon 按剧本应答，并在每个请求到达、应答之前调用 `hook`（传入请求行）
async fn scripted_with_hook(steps: Vec<Step>, hook: RequestHook) -> ScriptedDaemon {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut script = std::collections::VecDeque::new();
    script.extend(steps);
    let script = Arc::new(Mutex::new(script));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let mismatches = Arc::new(Mutex::new(Vec::new()));
    let (recorded, recorded_bodies, failed) = (
        Arc::clone(&requests),
        Arc::clone(&bodies),
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
            recorded.lock().unwrap().push(request.clone());
            recorded_bodies
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
            hook(&request);
            if step.as_ref().is_some_and(|step| step.stall) {
                std::future::pending::<()>().await;
            }
            if step.as_ref().is_some_and(|step| step.hang_up) {
                continue;
            }
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
        bodies,
        mismatches,
        server,
    }
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
