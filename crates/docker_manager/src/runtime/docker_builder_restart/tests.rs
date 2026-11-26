//! builder 重启恢复：真实 Bollard 客户端对着假 daemon，不依赖时序。
//!
//! 假 daemon 按剧本逐步响应，并记下每个请求的正文。替代容器绑定了什么、带了哪些环境变量和
//! label，都在创建请求的正文里，可以确定性地断言，不用靠 sleep 碰运气。

use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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
}

fn step(method: &'static str, contains: impl Into<String>, status: u16, body: Value) -> Step {
    Step {
        method,
        contains: contains.into(),
        status,
        body,
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

    /// 第 `index` 个请求的正文
    fn body(&self, index: usize) -> Value {
        let bodies = self.bodies.lock().unwrap();
        serde_json::from_str(&bodies[index])
            .unwrap_or_else(|error| panic!("request {index} has no JSON body: {error}"))
    }
}

async fn scripted(steps: Vec<Step>) -> ScriptedDaemon {
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
            recorded.lock().unwrap().push(request);
            recorded_bodies
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
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
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    Arc::new(crate::DockerManager {
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
    })
}

fn runtime(address: std::net::SocketAddr) -> DockerRuntime {
    DockerRuntime::new(docker_manager(address))
}

/// 用注入的挂载表构造运行时：RCoder 进程里的 `visible` 对应宿主机上的 `host`，不做容器自检
fn runtime_with_mounts(
    address: std::net::SocketAddr,
    visible: PathBuf,
    host: PathBuf,
) -> DockerRuntime {
    let resolver = crate::HostPathResolver::from_mount_table(vec![(visible, host)]).unwrap();
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

// ─── 带日志挂载的重启 ───────────────────────────────────────────────────────

/// 一次带日志挂载的重启。`host` 是 daemon 看到的路径，`visible` 是同一批位置在 RCoder 进程里
/// 的样子；两边各是真实的目录，靠注入的挂载表对应起来
struct LoggedRestart {
    root: tempfile::TempDir,
    workspace: tempfile::TempDir,
    template: BuilderRestartTemplate,
    archive: Archive,
    /// 原容器的日志目录（宿主机路径）
    old_directory: String,
    old_name: String,
}

impl LoggedRestart {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let container = "rcoder-app-builder-265";
        let old_name = format!("{container}-20251026080000");
        let old_directory = root
            .path()
            .join("host/container")
            .join(&old_name)
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::write(std::path::Path::new(&old_directory).join("old.log"), b"old").unwrap();
        let template = template_for(vec![
            host_path_volume(WORKSPACE_TARGET, &workspace.path().to_string_lossy()),
            host_path_volume(LOG_TARGET, &old_directory),
        ]);
        let archive = archive_for(
            &template,
            vec![
                format!("{old_directory}:{LOG_TARGET}:rw,z"),
                format!("{}:{WORKSPACE_TARGET}", workspace.path().display()),
            ],
            vec![
                format!("CONTAINER_LOGS_DIR={LOG_TARGET}"),
                format!("CONTAINER_LOG_NAME={old_name}"),
                "PATH=/usr/bin".into(),
            ],
        );
        Self {
            root,
            workspace,
            template,
            archive,
            old_directory,
            old_name,
        }
    }

    fn runtime(&self, address: std::net::SocketAddr) -> DockerRuntime {
        runtime_with_mounts(
            address,
            self.root.path().join("visible/logs"),
            self.root.path().join("host"),
        )
    }

    /// RCoder 进程里新建日志目录的位置
    fn visible_root(&self) -> PathBuf {
        self.root.path().join("visible/logs/container")
    }

    /// 替代容器绑定的日志目录名：取自创建请求里的 bind
    fn bound_log_name(&self, daemon: &ScriptedDaemon, request: usize) -> String {
        let binds = daemon.body(request)["HostConfig"]["Binds"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let target = format!(":{LOG_TARGET}");
        let source = binds
            .iter()
            .filter_map(Value::as_str)
            .find_map(|bind| bind.split_once(target.as_str()).map(|(source, _)| source))
            .unwrap_or_else(|| panic!("no bind targets {LOG_TARGET}: {binds:?}"));
        std::path::Path::new(source)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned()
    }

    fn expected_volumes(&self, log_source: &str) -> Vec<AppResourceIdentity> {
        vec![
            self.template.volumes[0].clone(),
            host_path_volume(LOG_TARGET, log_source),
        ]
    }

    /// 按 `self` 的模板和归档重放出替代容器
    async fn restore(&self, daemon: &ScriptedDaemon) -> Result<Vec<AppResourceIdentity>> {
        self.runtime(daemon.address)
            .replace_builder_container(&self.template, &self.archive, original(&self.template))
            .await
    }
}

// ─── 用例 ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_replacement_is_created_once_the_image_is_ready() {
    let workspace = tempfile::tempdir().unwrap();
    let template = restart_template(workspace.path());
    let archive = restart_archive(&template);
    let mut script = absent_original();
    script.extend(creation_prelude(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )));
    let daemon = scripted(script).await;
    let runtime = runtime(daemon.address);

    runtime
        .replace_builder_container(&template, &archive, original(&template))
        .await
        .unwrap();

    daemon.assert_followed(5);
    let body = daemon.body(4);
    assert_eq!(
        body["HostConfig"]["Binds"],
        json!([format!("{}:{WORKSPACE_TARGET}", workspace.path().display())]),
        "binds without a log mount are replayed verbatim"
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

    runtime
        .replace_builder_container(&template, &archive, original(&template))
        .await
        .unwrap();

    daemon.assert_followed(8);
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

// ─── 替代容器的日志目录 ─────────────────────────────────────────────────────

/// 日志挂载不照原样重放：替代容器绑定同级的新目录，环境变量和 label 跟着换，其它 bind 原样。
#[tokio::test]
async fn the_replacement_gets_a_new_log_directory() {
    let restart = LoggedRestart::new();
    let mut script = absent_original();
    script.extend(creation_prelude(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )));
    let daemon = scripted(script).await;

    let expected = restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(5);
    let body = daemon.body(4);
    let name = restart.bound_log_name(&daemon, 4);
    assert_eq!(
        name.strip_prefix("rcoder-app-builder-265-").map(str::len),
        Some(14),
        "the new directory keeps the container name and takes a new timestamp: {name}"
    );
    assert_ne!(name, restart.old_name);
    let new_directory = restart.root.path().join("host/container").join(&name);
    assert_eq!(
        body["HostConfig"]["Binds"],
        json!([
            format!("{}:{LOG_TARGET}:rw,z", new_directory.display()),
            format!("{}:{WORKSPACE_TARGET}", restart.workspace.path().display()),
        ]),
        "the log bind keeps its target and options; the workspace bind is replayed verbatim"
    );
    let env = body["Env"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        env.len(),
        4,
        "the archived environment is kept as it was, plus the execution domain stamp: {env:?}"
    );
    assert_eq!(
        env[..3],
        [
            json!(format!("CONTAINER_LOGS_DIR={LOG_TARGET}")),
            json!(format!("CONTAINER_LOG_NAME={name}")),
            json!("PATH=/usr/bin"),
        ],
        "CONTAINER_LOG_NAME follows the directory that is actually bound"
    );
    assert!(
        env[3]
            .as_str()
            .is_some_and(|entry| entry.starts_with("RCODER_EXECUTION_DOMAIN=")),
        "{env:?}"
    );
    assert_eq!(
        body["Labels"][RESTART_LOG_DIR],
        json!(new_directory.to_string_lossy()),
        "a resumed restore learns the directory from this label"
    );
    // 新目录是 RCoder 经自己的挂载视图建的
    assert!(restart.visible_root().join(&name).is_dir());
    // 旧目录原样留着，留给日志清理器
    assert_eq!(
        std::fs::read_dir(std::path::Path::new(&restart.old_directory))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        expected,
        restart.expected_volumes(&new_directory.to_string_lossy())
    );
}

/// CONTAINER_LOGS_DIR 与归档里的任何一条挂载都对不上（旧版本把带变量的容器路径原样写了进去）：
/// 认不出日志挂载就不换目录，bind 照归档重放。
#[tokio::test]
async fn a_log_directory_env_without_a_matching_bind_leaves_the_bind_as_archived() {
    let restart = LoggedRestart::new();
    let mut archive = restart.archive.clone();
    archive.env[0] = "CONTAINER_LOGS_DIR=/app/container-logs/{timestamp}".into();
    let mut script = absent_original();
    script.extend(creation_prelude(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )));
    let daemon = scripted(script).await;
    let runtime = restart.runtime(daemon.address);

    let expected = runtime
        .replace_builder_container(&restart.template, &archive, original(&restart.template))
        .await
        .unwrap();

    daemon.assert_followed(5);
    let body = daemon.body(4);
    assert_eq!(
        body["HostConfig"]["Binds"],
        json!([
            format!("{}:{LOG_TARGET}:rw,z", restart.old_directory),
            format!("{}:{WORKSPACE_TARGET}", restart.workspace.path().display()),
        ]),
        "nothing is rebound when the log mount cannot be recognised"
    );
    assert!(body["Labels"].get(RESTART_LOG_DIR).is_none());
    assert_eq!(expected, restart.template.volumes);
    assert!(
        !restart.visible_root().exists(),
        "no new directory is made for an unrecognised log mount"
    );
}

/// 旧日志目录已经不在了（停得久，被日志清理器回收）照样能恢复：替代容器用的本来就是新目录。
#[tokio::test]
async fn a_missing_old_log_directory_does_not_block_the_restart() {
    let restart = LoggedRestart::new();
    std::fs::remove_dir_all(&restart.old_directory).unwrap();
    let mut script = absent_original();
    script.extend(creation_prelude(step(
        "POST",
        "/containers/create",
        200,
        created(),
    )));
    let daemon = scripted(script).await;

    restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(5);
    assert_eq!(
        std::fs::read_dir(restart.visible_root()).unwrap().count(),
        1,
        "the replacement still gets a new directory"
    );
}

/// 上一次中断前建好的替代容器被原样接受，不再建第二个；它用的日志目录从 label 里读。
#[tokio::test]
async fn a_resumed_replacement_is_accepted_with_the_log_directory_it_was_given() {
    let restart = LoggedRestart::new();
    let given = "/srv/logs/container/rcoder-app-builder-265-20251026090000";
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &restart.template,
                json!({
                    RESTORE: restart.template.archive.uid.clone(),
                    RESTART_LOG_DIR: given,
                }),
            ),
        ),
    ])
    .await;

    let expected = restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(2);
    assert_eq!(expected, restart.expected_volumes(given));
}

/// 没有这个 label 的替代容器是旧版本建的：那时归档原样重放，它绑的就是归档里的目录。
#[tokio::test]
async fn a_resumed_replacement_from_an_older_version_is_verified_against_the_archive() {
    let restart = LoggedRestart::new();
    let daemon = scripted(vec![
        step("GET", ORIGINAL_UID, 404, no_such_container()),
        step(
            "GET",
            "rcoder-app-builder-",
            200,
            retained_replacement(
                &restart.template,
                json!({ RESTORE: restart.template.archive.uid }),
            ),
        ),
    ])
    .await;

    let expected = restart.restore(&daemon).await.unwrap();

    daemon.assert_followed(2);
    assert_eq!(expected, restart.template.volumes);
    assert!(
        !restart.visible_root().exists(),
        "nothing is created for a replacement that already exists"
    );
}

/// 替代容器没建成：刚为它建的日志目录没人用，删掉。
#[tokio::test]
async fn the_new_log_directory_is_discarded_when_the_replacement_cannot_be_created() {
    let restart = LoggedRestart::new();
    let mut script = absent_original();
    script.extend(creation_prelude(step(
        "POST",
        "/containers/create",
        500,
        json!({"message": "daemon refused"}),
    )));
    let daemon = scripted(script).await;

    let result = restart.restore(&daemon).await;

    assert!(
        matches!(&result, Err(Error::DockerError(message)) if message.contains("Create replacement builder")),
        "{result:?}"
    );
    daemon.assert_followed(5);
    assert_eq!(
        std::fs::read_dir(restart.visible_root()).unwrap().count(),
        0,
        "the directory made for a replacement that was never created is removed"
    );
}

/// 归档里的 bind 与模板记的挂载对不上：认不出哪一条是日志挂载，拒绝，什么都不建。
#[tokio::test]
async fn a_log_bind_that_does_not_match_the_witness_conflicts_and_creates_nothing() {
    let restart = LoggedRestart::new();
    let mut archive = restart.archive.clone();
    archive.binds[0] = format!("/elsewhere/logs:{LOG_TARGET}");
    let daemon = scripted(absent_original()).await;
    let runtime = restart.runtime(daemon.address);

    let result = runtime
        .replace_builder_container(&restart.template, &archive, original(&restart.template))
        .await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Restart log bind does not match")),
        "{result:?}"
    );
    daemon.assert_followed(2);
    assert!(
        !restart.visible_root().exists(),
        "the directory is only created after every check has passed"
    );
}

/// 工作区核对不过时不建日志目录。
#[tokio::test]
async fn no_log_directory_is_created_when_another_bind_source_is_missing() {
    let restart = LoggedRestart::new();
    std::fs::remove_dir_all(restart.workspace.path()).unwrap();
    let daemon = scripted(absent_original()).await;
    let runtime = restart.runtime(daemon.address);

    let result = runtime
        .replace_builder_container(
            &restart.template,
            &restart.archive,
            original(&restart.template),
        )
        .await;

    assert!(
        matches!(&result, Err(Error::Conflict(message)) if message.contains("Restart bind source is missing")),
        "{result:?}"
    );
    daemon.assert_followed(2);
    assert!(!restart.visible_root().exists());
}
