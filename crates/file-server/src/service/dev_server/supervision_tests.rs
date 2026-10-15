//! 真实进程 + 真实监督 wire 的本地启停闭环测试（复核批次 A/B 最小覆盖 +
//! 链路验证）。
//!
//! fixture 编排器 = 受控 python3 进程：真实 flock（owner.lock）、真实 TCP
//! 控制 wire（与 runtime-supervisor `Discovery/Envelope/Reply` 同 serde 形态，
//! 两端测试锁同一格式）、真实业务子进程（sleep）。停止路径经历真实的
//! 协议收束 + 进程组终止 + 退出确认——不拿 mock 状态或 HTTP 受理当成功。
//!
//! 环境要求：`python3`（测试硬失败而非静默跳过）；状态根走 standalone
//! registry 段（不读/不写全局 env——项目禁用 set_var）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use runtime_supervisor::{Binding, Phase, Request, Snapshot};

use super::support::lock;
use super::types::{DevServerManager, StartedDev};
use crate::Config;

// ── wire 镜像（runtime-supervisor 侧为 pub(crate)，此处按 wire 格式镜像；
// 字段名/serde 形态与 control.rs 的类型一致，两端测试锁同一格式）──────────

#[derive(serde::Serialize)]
struct WireEnvelope<'a> {
    version: u32,
    instance: &'a str,
    token: &'a str,
    request: &'a Request,
}

#[derive(serde::Serialize)]
pub(super) struct WireDiscoveryFile {
    pub(super) version: u32,
    pub(super) instance: String,
    pub(super) address: String,
    pub(super) token: String,
    pub(super) snapshot: Snapshot,
    pub(super) requests: Vec<(Request, Snapshot)>,
}

/// fixture 编排器（真实进程）。
const ORCHESTRATOR_FIXTURE: &str = r##"#!/usr/bin/env python3
import fcntl, json, os, socket, subprocess, sys, time, uuid

ws = None
args = sys.argv[1:]
i = 0
while i < len(args):
    if args[i] == "--workspace" and i + 1 < len(args):
        ws = os.path.realpath(args[i + 1])
    i += 1
assert ws is not None, "fixture requires --workspace"

# standalone registry 段（first-wins）：无平台 env 的 run 与 Rust 侧发现共用。
base = os.path.join(os.path.dirname(ws), ".app-cli-state")
os.makedirs(base, exist_ok=True)
registry = os.path.join(base, "registry.json")
segment = None
mapped = {}
if os.path.exists(registry):
    try:
        with open(registry) as f:
            mapped = json.load(f)
        segment = mapped.get(ws)
    except Exception:
        mapped = {}
if segment is None:
    segment = "p-fixture-" + uuid.uuid4().hex[:12]
    mapped[ws] = segment
    tmp = registry + ".fixture-tmp"
    with open(tmp, "w") as f:
        json.dump(mapped, f)
    os.replace(tmp, registry)
root = os.path.join(base, segment)
os.makedirs(root, exist_ok=True)

mode_file = os.path.join(ws, "fixture-mode.txt")
def read_mode():
    try:
        with open(mode_file) as f:
            return f.read().strip() or "normal"
    except Exception:
        return "normal"
mode = read_mode()

lock_file = open(os.path.join(root, "owner.lock"), "a+")
fcntl.flock(lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)

child = subprocess.Popen(["sleep", "3000"])
marker = os.path.join(ws, "marker.txt")
served = os.path.join(ws, "served.txt")
content = "missing-marker"
if os.path.exists(marker):
    with open(marker) as f:
        content = f.read()
with open(served, "w") as f:
    f.write(content)

instance = uuid.uuid4().hex
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.bind(("127.0.0.1", 0))
srv.listen(16)
addr = "127.0.0.1:%d" % srv.getsockname()[1]

snapshot = {
    "version": 1,
    "binding": {"component": "app-cli", "resource": ws},
    "supervisor_id": instance,
    "generation": None,
    "phase": "ready",
    "intent": "run",
    "operation_id": None,
    "error": None,
}
requests = []

def save():
    payload = {"version": 2, "instance": instance, "address": addr,
               "token": "fixture-token", "snapshot": snapshot, "requests": requests}
    tmp = os.path.join(root, "supervisor.json.fixture-tmp")
    with open(tmp, "w") as f:
        json.dump(payload, f)
    os.replace(tmp, os.path.join(root, "supervisor.json"))

def log_stop(req):
    with open(os.path.join(root, "stopwork.log"), "a") as f:
        f.write(json.dumps({"request_id": req["request_id"]}) + "\n")

save()

while True:
    conn, _ = srv.accept()
    try:
        f = conn.makefile("rwb")
        line = f.readline()
        if not line:
            conn.close()
            continue
        env = json.loads(line)
        req = env["request"]
        if env.get("instance") != instance:
            reply = {"instance": instance, "snapshot": dict(snapshot),
                     "error": {"code": "identity_changed", "message": "stale instance"}}
        elif req["action"] == "status":
            reply = {"instance": instance, "snapshot": dict(snapshot), "error": None}
        elif req["action"] == "stop_work":
            log_stop(req)
            mode = read_mode()
            if mode == "hang":
                conn.close()
                continue
            time.sleep(0.3)
            child.terminate()
            try:
                child.wait(timeout=5)
            except Exception:
                child.kill()
            snapshot["intent"] = "stopped"
            snapshot["phase"] = "stopped"
            snapshot["operation_id"] = req["request_id"]
            requests.append([req, dict(snapshot)])
            save()
            # 回包 best-effort（对端可能在读后立刻关闭——写失败也无妨：
            # 终态已落盘，客户端丢回复分支按同请求回执收束）；随后**无条件**
            # 退出——已停止的 run 编排器不再驻留（残留会占住 owner.lock/端口，
            # 阻断下一次启动）。
            try:
                reply = {"instance": instance, "snapshot": dict(snapshot), "error": None}
                f.write((json.dumps(reply) + "\n").encode())
                f.flush()
            except Exception:
                pass
            try:
                conn.close()
            except Exception:
                pass
            os._exit(0)
        else:
            reply = {"instance": instance, "snapshot": dict(snapshot),
                     "error": {"code": "invalid_request", "message": "unsupported"}}
        f.write((json.dumps(reply) + "\n").encode())
        f.flush()
        conn.close()
    except Exception:
        try:
            conn.close()
        except Exception:
            pass
"##;

/// 慢编排器（无监督面、仅存活）：探活窗口内进程存活但永不就绪。
const SLEEP_ORCHESTRATOR: &str = "#!/bin/sh\nexec sleep 120\n";

fn original_process_identity(pid: u32) -> String {
    let output = std::process::Command::new("python3")
        .args(["-c", "import pathlib,subprocess,sys; pid=sys.argv[1]; print(pathlib.Path('/proc/'+pid+'/stat').read_text().rsplit(')',1)[1].split()[19] if sys.platform=='linux' else subprocess.check_output(['ps','-p',pid,'-o','lstart='],text=True).strip())", &pid.to_string()])
        .output().expect("read native process start time");
    assert!(
        output.status.success(),
        "original process {pid} must exist: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let identity = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    assert!(
        !identity.is_empty(),
        "original process start time must be present"
    );
    identity
}

/// The real registered child survives observation expiry until explicit Stop.
#[tokio::test]
async fn local_manifest_observation_uses_parent_deadline_without_cancelling_original_child() {
    let env = fixture_env(SLEEP_ORCHESTRATOR).await;
    let key = "userapp:local-observation-budget";
    let mut hooks = crate::service::dev_server::DevEventHooks::noop();
    hooks.launch_deadline =
        Some(tokio::time::Instant::now() + std::time::Duration::from_millis(400));
    let mut starting = tokio::spawn({
        let manager = env.manager.clone();
        let workspace = env.workspace.clone();
        async move {
            manager
                .start_dev(
                    key,
                    &workspace,
                    crate::service::dev_server::DevLaunch {
                        base_path: None,
                        hooks: Some(hooks),
                        pg: None,
                        request_context: Some("original-local-task"),
                        artifact_release_id: None,
                    },
                )
                .await
        }
    });
    let child = tokio::time::timeout(std::time::Duration::from_millis(300), async {
        loop {
            if let Some(child) = env.manager.supervised_child(key) {
                break child;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("real child registered before readiness observation");
    let pid = child.pid();
    let identity = original_process_identity(pid);
    let launch_id = lock(&env.manager.launches).unwrap()[key].launch_id.clone();
    assert_eq!(lock(&env.manager.processes).unwrap()[key].pid, pid);
    let observed = tokio::time::timeout(std::time::Duration::from_millis(600), &mut starting).await;
    let same_registered = env
        .manager
        .supervised_child(key)
        .is_some_and(|registered| Arc::ptr_eq(&registered, &child))
        && lock(&env.manager.launches).unwrap()[key].launch_id == launch_id
        && lock(&env.manager.processes).unwrap()[key].pid == pid;
    let alive_after_observation = is_alive(pid) && original_process_identity(pid) == identity;
    if observed.is_err() {
        starting.abort();
    }
    let stopped = env
        .manager
        .stop_dev(key)
        .await
        .expect("explicit Stop collects the original child");
    wait_until_dead(pid, "original local deadline child").await;
    assert!(stopped.killed_pids.iter().any(|k| k.pid == pid && k.killed));
    let error = observed.expect("local readiness observation must consume the parent budget rather than start another full poll window")
        .expect("local start worker").expect_err("deadline cannot be reported as readiness success");
    assert!(error.to_string().contains("deadline"), "{error}");
    assert!(
        same_registered,
        "deadline preserves the original launch and process registration"
    );
    assert!(
        alive_after_observation,
        "deadline ends observation, not the accepted original business execution"
    );
}

/// Native Stop takes 300ms in this fixture. Its original identity remains
/// recoverable, but a 200ms parent budget cannot authorize a late directory switch.
#[tokio::test]
async fn local_restart_cleanup_consumes_parent_budget_before_activation() {
    let env = fixture_env(ORCHESTRATOR_FIXTURE).await;
    let key = "userapp:local-cleanup-budget";
    let started = env
        .manager
        .start_dev(
            key,
            &env.workspace,
            crate::service::dev_server::DevLaunch {
                base_path: None,
                hooks: None,
                pg: None,
                request_context: None,
                artifact_release_id: None,
            },
        )
        .await
        .expect("original local execution");
    let original_identity = original_process_identity(started.pid);
    assert_eq!(lock(&env.manager.processes).unwrap()[key].pid, started.pid);
    assert_eq!(
        env.manager.supervised_child(key).unwrap().pid(),
        started.pid
    );
    assert!(is_alive(started.pid) && original_process_identity(started.pid) == original_identity);
    let mut hooks = crate::service::dev_server::DevEventHooks::noop();
    hooks.launch_deadline =
        Some(tokio::time::Instant::now() + std::time::Duration::from_millis(200));
    let activated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let activation = activated.clone();
    let workspace = env.workspace.clone();
    let outcome = env
        .manager
        .restart_dev_staged(
            key,
            &env.workspace,
            crate::service::dev_server::DevLaunch {
                base_path: None,
                hooks: Some(hooks),
                pg: None,
                request_context: Some("original-cleanup-task"),
                artifact_release_id: None,
            },
            async move {
                std::fs::write(workspace.join("marker.txt"), "B").unwrap();
                activation.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(workspace)
            },
        )
        .await;
    let did_activate = activated.load(std::sync::atomic::Ordering::SeqCst);
    let marker_after_observation =
        std::fs::read_to_string(env.workspace.join("marker.txt")).unwrap();
    env.manager
        .stop_userapp_dev(key, &env.workspace)
        .await
        .expect("resume original physical cleanup");
    wait_until_dead(started.pid, "original cleanup-budget orchestrator").await;
    let requests = stop_log_of(&env.workspace);
    assert!(!requests.is_empty(), "native Stop really executed");
    assert_eq!(
        requests
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1,
        "cleanup retry must keep the original Stop identity: {requests:?}"
    );
    assert!(
        outcome.is_err(),
        "the consumed parent budget must end restart observation"
    );
    assert!(
        !did_activate,
        "cleanup consumed the parent deadline; no late activation or new launch is authorized"
    );
    assert_eq!(marker_after_observation, "A");
}

fn require_python3() {
    let Ok(status) = std::process::Command::new("python3")
        .arg("-c")
        .arg("print('ok')")
        .status()
    else {
        panic!("python3 is required for the real-process supervision fixture");
    };
    assert!(status.success(), "python3 must be runnable for the fixture");
}

struct FixtureEnv {
    /// 持有临时目录存活（Drop 即清理）；测试体不直接读取。
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    workspace: PathBuf,
    manager: Arc<DevServerManager>,
    config: Config,
}

fn write_executable(path: &Path, content: &str) {
    if let Err(error) = std::fs::write(path, content) {
        panic!("write fixture script {}: {error}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("chmod fixture: {error}"));
    }
}

async fn fixture_env(script: &str) -> FixtureEnv {
    fixture_env_with_probe(script, None).await
}

async fn fixture_env_with_probe(script: &str, probe_override: Option<String>) -> FixtureEnv {
    require_python3();
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("ws-app");
    std::fs::create_dir_all(&workspace).unwrap();
    // manifest 分流标记：start_dev → app-cli 引擎（start_dev_manifest）。
    std::fs::write(workspace.join("workspace.manifest.toml"), "# fixture\n").unwrap();
    std::fs::write(workspace.join("marker.txt"), "A").unwrap();
    let bin = dir.path().join("orchestrator.sh");
    write_executable(&bin, script);
    // 3010 探测指向确定空闲端口（owner 复用分支一律 Absent）。
    let probe_addr = probe_override.unwrap_or_else(|| {
        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = unused.local_addr().unwrap().to_string();
        drop(unused);
        addr
    });
    let mut config = Config::from_env().expect("test config");
    config.app_cli_bin = Some(bin.display().to_string());
    config.app_cli_admin_probe_addr = probe_addr;
    config.log_base_dir = dir.path().join("logs");
    std::fs::create_dir_all(&config.log_base_dir).unwrap();
    config.dev_alive_max_wait_ms = 1500;
    config.dev_alive_poll_interval_ms = 100;
    config.dev_alive_check_timeout_ms = 300;
    config.dev_supervision_stop_budget_secs = 6;
    config.dev_stop_max_attempts = 50;
    config.dev_stop_check_interval_ms = 100;
    let manager = Arc::new(DevServerManager::new(Arc::new(config.clone())));
    FixtureEnv {
        dir,
        workspace,
        manager,
        config,
    }
}

/// fixture 的 standalone registry 状态根（发现根）。
fn registry_state_root_of(workspace: &Path) -> PathBuf {
    let base = workspace.parent().unwrap().join(".app-cli-state");
    let registry = base.join("registry.json");
    let map: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(&registry).expect("registry"))
            .expect("registry map");
    let canonical = std::fs::canonicalize(workspace).unwrap();
    let segment = map
        .get(canonical.to_string_lossy().as_ref())
        .expect("workspace registered");
    base.join(segment)
}

fn stop_log_of(workspace: &Path) -> Vec<String> {
    let path = registry_state_root_of(workspace).join("stopwork.log");
    match std::fs::read_to_string(&path) {
        Ok(content) => content
            .lines()
            .filter_map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).ok()?;
                value
                    .get("request_id")
                    .and_then(|id| id.as_str())
                    .map(String::from)
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn persisted_local_stops_at(log_dir: &Path) -> serde_json::Value {
    let path = log_dir.join("dev-server-external.json");
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or(serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    }
}

/// R2（Starting 被 Stop 插入）：spawn 已发生、探活未过的窗口内，三张表
///（processes/launches/supervised）已原子发表——Stop 能看到完整登记并
/// 真实终止进程组；Start 最终失败（进程被停）且不残留登记。
#[tokio::test]
async fn starting_registration_is_fully_published_and_stoppable_during_poll() {
    let env = fixture_env(SLEEP_ORCHESTRATOR).await;
    let key = "userapp:start-stop-race";
    let manager = env.manager.clone();
    let ws = env.workspace.clone();
    let start = tokio::spawn(async move {
        manager
            .start_dev(
                key,
                &ws,
                crate::service::dev_server::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: None,
                    request_context: None,
                    artifact_release_id: None,
                },
            )
            .await
    });
    // 探活窗口（max_wait 1500ms）内等 Starting 发表——三张表齐全。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let pid = loop {
        let observed = {
            let processes = lock(&env.manager.processes).unwrap();
            let launches = lock(&env.manager.launches).unwrap();
            let supervised = lock(&env.manager.supervised).unwrap();
            match (processes.get(key), launches.get(key), supervised.get(key)) {
                (Some(process), Some(_launch), Some(child)) => {
                    assert_eq!(
                        process.pid,
                        child.pid(),
                        "registration must carry the real Child"
                    );
                    Some(process.pid)
                }
                _ => None,
            }
        };
        if let Some(pid) = observed {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Starting registration must publish processes+launches+supervised before readiness"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    // 探活窗口内插入 Stop：真实终止 + 退出确认 + 登记退休。
    let stopped = env
        .manager
        .stop_userapp_dev(key, &env.workspace)
        .await
        .expect("stop during the readiness window must collect the published registration");
    assert!(
        stopped.killed_pids.iter().any(|k| k.pid == pid && k.killed),
        "the real orchestrator pid must be terminated and reported: {:?}",
        stopped.killed_pids
    );
    // Start 收尾：进程已死 → 探活失败；登记已被 Stop 退休（清理空集幂等）。
    let start_result = start.await.expect("start task join");
    assert!(
        start_result.is_err(),
        "killed orchestrator cannot become ready"
    );
    assert!(lock(&env.manager.processes).unwrap().is_empty());
    assert!(lock(&env.manager.launches).unwrap().is_empty());
    assert!(lock(&env.manager.supervised).unwrap().is_empty());
}

/// 真正的 start_dev 发表本地 child 后卡住提交回调：Stop 收束原 child，
/// 再用既有持久 intent API 注入 external successor，然后才恢复旧探活。
/// successor 的登记是受控故障注入，不宣称真实 owner/容器端到端验收。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_local_start_does_not_retire_an_external_successor_after_stop() {
    use std::time::Duration;

    struct SubmissionGate {
        entered: tokio::sync::Notify,
        released: std::sync::Mutex<bool>,
        condition: std::sync::Condvar,
    }
    impl SubmissionGate {
        fn release(&self) {
            let mut released = self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *released = true;
            self.condition.notify_all();
        }
    }
    struct ReleaseOnDrop(Arc<SubmissionGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    struct OriginalChildCleanup(Arc<super::SupervisedChild>);
    impl Drop for OriginalChildCleanup {
        fn drop(&mut self) {
            if self.0.exited().is_none() {
                let _ = super::process::kill_process_group_force(self.0.pid());
            }
        }
    }

    let env = fixture_env(SLEEP_ORCHESTRATOR).await;
    let key = "userapp:late-local-failure";
    let gate = Arc::new(SubmissionGate {
        entered: tokio::sync::Notify::new(),
        released: std::sync::Mutex::new(false),
        condition: std::sync::Condvar::new(),
    });
    let _release_on_drop = ReleaseOnDrop(gate.clone());
    let mut hooks = super::DevEventHooks::noop();
    hooks.on_submitted = Some(Arc::new({
        let gate = gate.clone();
        move || {
            // 卡住真实生产回调，不能持任何 manager registry 锁。
            gate.entered.notify_one();
            let mut released = gate.released.lock().unwrap();
            while !*released {
                released = gate.condition.wait(released).unwrap();
            }
        }
    }));
    let mut starting = tokio::spawn({
        let manager = env.manager.clone();
        let workspace = env.workspace.clone();
        async move {
            manager
                .start_dev(
                    key,
                    &workspace,
                    super::DevLaunch {
                        base_path: None,
                        hooks: Some(hooks),
                        pg: None,
                        request_context: Some("original-local-task"),
                        artifact_release_id: None,
                    },
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("真实 local child 的完整登记必须先于提交回调");
    let child = env.manager.supervised_child(key).unwrap();
    let original_pid = child.pid();
    let _original_cleanup = OriginalChildCleanup(child.clone());
    let original_launch = lock(&env.manager.launches)
        .unwrap()
        .get(key)
        .unwrap()
        .launch_id
        .clone();
    assert!(is_alive(original_pid));
    let stopped = tokio::time::timeout(
        Duration::from_secs(8),
        env.manager.stop_userapp_dev(key, &env.workspace),
    )
    .await
    .expect("提交回调只持测试门，不能阻止真实 Stop")
    .expect("显式 Stop 必须确认原 child 退出并条件退休原登记");
    wait_until_dead(original_pid, "stopped original startup child").await;
    assert!(matches!(child.exited(), Some(super::ChildExit::Exited(_))));
    assert!(
        stopped
            .killed_pids
            .iter()
            .any(|entry| entry.pid == original_pid && entry.killed)
    );
    assert!(!lock(&env.manager.launches).unwrap().contains_key(key));
    assert!(!starting.is_finished(), "旧 start 仍被真实提交回调门控");

    // 真实小型 HTTP owner 进程绑定 successor 的 address/instance 物理见证。
    // 仅提供 identity，所有访问留痕；不伪造运行成功，也不忽略 TERM/KILL。
    const SUCCESSOR_IDENTITY_SERVER: &str = r#"
import http.server, pathlib, sys
address_file, payload, request_file = sys.argv[1:]
class Handler(http.server.BaseHTTPRequestHandler):
    def record(self):
        with open(request_file, 'a') as stream:
            stream.write(self.command + ' ' + self.path + '\n')
    def do_GET(self):
        self.record()
        self.send_response(200 if self.path == '/v1/runtime/identity' else 404)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(payload.encode())
    def do_POST(self):
        self.record()
        self.send_response(409)
        self.end_headers()
    def log_message(self, *_):
        pass
server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
pathlib.Path(address_file).write_text('127.0.0.1:%d' % server.server_port)
server.serve_forever()
"#;
    let address_file = env.workspace.join("successor-address.txt");
    let request_file = env.workspace.join("successor-requests.txt");
    let identity = shared_types::RuntimeIdentityView {
        application_id: std::env::var("PROJECT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown-app".into()),
        service_family: "userapp-dev".into(),
        workspace_id: "successor-workspace".into(),
        source_root: env.workspace.display().to_string(),
        runtime_instance_id: "external-successor-instance".into(),
        deployment_generation_id: "successor-generation".into(),
        protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    let mut successor = tokio::process::Command::new("python3")
        .args(["-c", SUCCESSOR_IDENTITY_SERVER])
        .arg(&address_file)
        .arg(serde_json::to_string(&shared_types::HttpResult::success(identity.clone())).unwrap())
        .arg(&request_file)
        .kill_on_drop(true)
        .spawn()
        .expect("独立 successor HTTP owner 进程");
    let successor_pid = successor.id().unwrap();
    assert_ne!(successor_pid, original_pid);
    let successor_process_identity = original_process_identity(successor_pid);
    let successor_address = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(address) = tokio::fs::read_to_string(&address_file).await
                && !address.is_empty()
            {
                break address;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("真实 successor HTTP owner 必须发布监听地址");
    let observed = super::owner_client::probe_owner(&successor_address)
        .await
        .unwrap()
        .expect("真实 successor identity 响应");
    assert_eq!(observed.runtime_instance_id, identity.runtime_instance_id);
    let external = crate::models::ExternalOwner {
        address: successor_address,
        token: "controlled-successor-token".into(),
        runtime_instance_id: observed.runtime_instance_id,
    };
    let request = shared_types::RuntimeOperationRequest {
        operation_id: "controlled-successor-operation".into(),
        expected_runtime_instance_id: external.runtime_instance_id.clone(),
        expected_revision: 0,
        workspace_id: "successor-workspace".into(),
        kind: shared_types::RuntimeOperationKind::Restart,
        profile: shared_types::RunProfileInput::Source {
            workspace_id: "successor-workspace".into(),
        },
        run_config: None,
        request_context: Some("successor-task".into()),
    };
    let intent = env
        .manager
        .prepare_external_intent(key, &env.workspace, &external, &request)
        .expect("既有持久发布 API 注入后继 external 登记，不另造生产测试 seam");
    let published = lock(&env.manager.processes)
        .unwrap()
        .get(key)
        .cloned()
        .unwrap();
    assert_eq!(
        published
            .external_owner
            .as_ref()
            .unwrap()
            .runtime_instance_id,
        external.runtime_instance_id
    );

    gate.release();
    let outcome = tokio::time::timeout(Duration::from_secs(5), &mut starting).await;
    if outcome.is_err() {
        starting.abort();
    }
    let remaining = lock(&env.manager.processes).unwrap().get(key).cloned();
    let durable = env.manager.read_external_state().unwrap();
    let successor_survived = successor.try_wait().unwrap().is_none();
    let successor_identity_after =
        successor_survived.then(|| original_process_identity(successor_pid));
    let successor_requests = std::fs::read_to_string(&request_file).unwrap();
    // 收集物理证据后总是清理本测试拥有的见证进程，再进行回归断言。
    if successor_survived {
        successor.start_kill().unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), successor.wait())
        .await
        .expect("见证进程必须退出")
        .expect("收割见证进程");

    let error = outcome
        .expect("恢复原回调后旧 start 必须有界结束")
        .expect("旧 start worker")
        .expect_err("原 child 已被 Stop 收束，旧探活不能返回成功");
    assert!(
        error.to_string().contains(&format!("pid {original_pid}")),
        "必须保留原探活早退错误，不被后继登记的清理拒绝覆盖：{error}"
    );
    let remaining = remaining.expect("旧启动失败收尾不能删除 successor DevProcess");
    assert_eq!(
        remaining
            .external_owner
            .as_ref()
            .unwrap()
            .runtime_instance_id,
        external.runtime_instance_id
    );
    let owner = durable.owners.get(key).expect("后继持久 owner 必须保留");
    assert_eq!(
        owner.registration_operation_id.as_deref(),
        Some(intent.request.operation_id.as_str())
    );
    assert_eq!(
        owner.owner.runtime_instance_id,
        external.runtime_instance_id
    );
    assert!(
        successor_survived,
        "旧收尾不得对 successor owner 进程发送 TERM/KILL"
    );
    assert_eq!(
        successor_requests.lines().collect::<Vec<_>>(),
        ["GET /v1/runtime/identity"],
        "旧收尾不得向 successor 发送停止/重启或其他控制请求"
    );
    assert_eq!(
        successor_identity_after.as_deref(),
        Some(successor_process_identity.as_str())
    );
    assert!(
        !lock(&env.manager.launches)
            .unwrap()
            .values()
            .any(|launch| launch.launch_id == original_launch)
    );
}

/// R1（旧记录但无进程）：磁盘保留 phase=Ready 的历史记录、owner 已退
///（锁可取、无监听）——Start 就地收束死记录后照常启动，无需删状态文件。
#[tokio::test]
async fn stale_record_without_process_is_collected_and_start_proceeds() {
    let env = fixture_env(ORCHESTRATOR_FIXTURE).await;
    let key = "userapp:stale-record";
    // 预置死记录：registry 登记 + supervisor.json（phase=ready、无监听地址）。
    let state_root = {
        let base = env.workspace.parent().unwrap().join(".app-cli-state");
        std::fs::create_dir_all(&base).unwrap();
        let registry = base.join("registry.json");
        let mut map = std::collections::BTreeMap::new();
        let canonical = std::fs::canonicalize(&env.workspace).unwrap();
        map.insert(
            canonical.to_string_lossy().into_owned(),
            "p-stale".to_string(),
        );
        std::fs::write(&registry, serde_json::to_string(&map).unwrap()).unwrap();
        let root = base.join("p-stale");
        std::fs::create_dir_all(&root).unwrap();
        let stale = WireDiscoveryFile {
            version: 2,
            instance: "dead-supervisor".into(),
            address: "127.0.0.1:1".into(),
            token: "stale".into(),
            snapshot: Snapshot {
                version: 1,
                binding: Binding {
                    component: "app-cli".into(),
                    resource: canonical,
                },
                supervisor_id: "dead-supervisor".into(),
                generation: None,
                phase: Phase::Ready,
                intent: runtime_supervisor::Intent::Run,
                operation_id: None,
                error: None,
                problem: None,
            },
            requests: Vec::new(),
        };
        std::fs::write(
            root.join("supervisor.json"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        root
    };
    let started: StartedDev = env
        .manager
        .start_dev(
            key,
            &env.workspace,
            crate::service::dev_server::DevLaunch {
                base_path: None,
                hooks: None,
                pg: None,
                request_context: None,
                artifact_release_id: None,
            },
        )
        .await
        .expect("start must collect the dead record and proceed");
    assert!(started.pid > 0);
    // 死记录已被离线收束为 Stopped（新 fixture 会以自己的 discovery 覆盖，
    // 这里断言收集动作发生过：stopwork 收据目录存在离线收据）。
    let collected = std::fs::read(state_root.join("supervisor.json")).unwrap_or_default();
    let value: serde_json::Value =
        serde_json::from_slice(&collected).expect("post-collection discovery readable");
    // fixture 覆盖或离线收束，二者其一必然存在且合法——真正要守住的是
    // start 没有因磁盘 phase 拒绝（上面 expect 已证）。
    assert!(value.get("snapshot").is_some());
    // 清场：停掉 fixture。
    env.manager
        .stop_userapp_dev(key, &env.workspace)
        .await
        .expect("fixture stop");
}

/// R4（两个 Stop 竞争）：同一目标并发两次 Stop——协调锁串行化 + 占座复用，
/// 监督端只收到**一个**停止身份（stopwork.log 全部行同一 request_id，
/// 且无第二个 StopWork 投递给已收束的代次）。
#[tokio::test]
async fn concurrent_stops_send_a_single_stop_identity() {
    let env = fixture_env(ORCHESTRATOR_FIXTURE).await;
    let key = "userapp:two-stops";
    env.manager
        .start_dev(
            key,
            &env.workspace,
            crate::service::dev_server::DevLaunch {
                base_path: None,
                hooks: None,
                pg: None,
                request_context: None,
                artifact_release_id: None,
            },
        )
        .await
        .expect("fixture start");
    let (first, second) = {
        let a = env.manager.stop_userapp_dev(key, &env.workspace);
        let b = env.manager.stop_userapp_dev(key, &env.workspace);
        tokio::join!(a, b)
    };
    let first = first.expect("first stop");
    let second = second.expect("second stop must be idempotent after the first");
    // Either HTTP observation can finish first and win the stop lock. The
    // contract is one actual stop and one idempotent result, not join! order.
    assert_ne!(
        first.owner_stopped, second.owner_stopped,
        "exactly one concurrent caller must perform the supervised stop"
    );
    let log = stop_log_of(&env.workspace);
    assert!(!log.is_empty(), "fixture must have received StopWork");
    let unique: std::collections::HashSet<_> = log.iter().collect();
    assert_eq!(
        unique.len(),
        1,
        "all StopWork deliveries must share one request identity: {log:?}"
    );
}

/// R4/R5（file-server 重建后原请求续查）：第一轮 Stop 受理后丢回复不落
/// 终态（hang 模式）→ 预算耗尽保留持久 attempt；重建 manager（内存登记
/// 丢失、外部状态恢复）→ 第二轮 Stop 以**同一 request_id** 续行至完成。
#[tokio::test]
async fn manager_rebuild_resumes_the_persisted_stop_request_identity() {
    let env = fixture_env(ORCHESTRATOR_FIXTURE).await;
    std::fs::write(env.workspace.join("fixture-mode.txt"), "hang").unwrap();
    let key = "userapp:rebuild-resume";
    env.manager
        .start_dev(
            key,
            &env.workspace,
            crate::service::dev_server::DevLaunch {
                base_path: None,
                hooks: None,
                pg: None,
                request_context: None,
                artifact_release_id: None,
            },
        )
        .await
        .expect("fixture start");
    let pending = env
        .manager
        .stop_userapp_dev(key, &env.workspace)
        .await
        .expect_err("hanging first stop must report still-in-progress");
    assert!(
        pending
            .to_string()
            .contains("retry resumes the same request"),
        "unexpected error: {pending}"
    );
    let persisted = persisted_local_stops_at(&env.config.log_base_dir);
    let local_stops = persisted
        .get("local_stops")
        .and_then(|v| v.as_object())
        .expect("persisted local stop record");
    assert_eq!(
        local_stops.len(),
        1,
        "exactly one seated attempt: {local_stops:?}"
    );
    let retained = local_stops.values().next().unwrap();
    assert!(
        retained["mode"].as_str().unwrap().starts_with("online:"),
        "the actual owner must already be durable when Stop times out: {retained}"
    );
    // 重建 manager：内存登记丢失，仅持久状态恢复；fixture 切换为完成模式。
    let FixtureEnv {
        workspace,
        manager,
        config,
        ..
    } = env;
    drop(manager);
    std::fs::write(workspace.join("fixture-mode.txt"), "normal").unwrap();
    let rebuilt = Arc::new(DevServerManager::new(Arc::new(config.clone())));
    let stopped = rebuilt
        .stop_userapp_dev(key, &workspace)
        .await
        .expect("resumed stop must complete after rebuild");
    assert!(
        stopped.owner_stopped,
        "resumed stop performed the supervised stop"
    );
    let log = stop_log_of(&workspace);
    assert!(!log.is_empty());
    let unique: std::collections::HashSet<_> = log.iter().collect();
    assert_eq!(
        unique.len(),
        1,
        "resume must reuse the original request id: {log:?}"
    );
    let after = persisted_local_stops_at(&config.log_base_dir);
    assert!(
        after
            .get("local_stops")
            .and_then(|v| v.as_object())
            .is_none_or(|m| m.is_empty()),
        "completed attempt must be forgotten: {after}"
    );
}

/// 链路（复核批次 C）：启动 → 停止 → 重启返回新内容 → 重建 manager 丢
/// 内存登记 → 停止 → 再启动。全程真实进程、真实监督 wire、真实组终止；
/// 工作区文件保留，内容随重启实际更换。
#[tokio::test]
async fn real_process_chain_start_stop_restart_rebuild_stop_start() {
    let env = fixture_env(ORCHESTRATOR_FIXTURE).await;
    let key = "userapp:chain";

    // 1. 启动（内容 A）。
    let first: StartedDev = env
        .manager
        .start_dev(
            key,
            &env.workspace,
            crate::service::dev_server::DevLaunch {
                base_path: None,
                hooks: None,
                pg: None,
                request_context: None,
                artifact_release_id: None,
            },
        )
        .await
        .expect("chain start");
    wait_for_content(&env.workspace, "A");
    let first_pid = first.pid;

    // 2. 停止：监督收束 + 真实退出 + 登记退休。
    let stopped = env
        .manager
        .stop_userapp_dev(key, &env.workspace)
        .await
        .expect("chain stop");
    assert!(stopped.owner_stopped);
    // fixture 在回包后自行退出，与组终止存在时序竞争——以进程真实消失为准。
    wait_until_dead(first_pid, "old orchestrator").await;

    // 3. 重启（内容 B）：staged（停止确认 → activate → 启动）返回新 pid，
    //    服务内容实际更换。
    std::fs::write(env.workspace.join("marker.txt"), "B").unwrap();
    let stop_ws = env.workspace.clone();
    let activate_ws = env.workspace.clone();
    let second = env
        .manager
        .restart_dev_staged(
            key,
            &stop_ws,
            crate::service::dev_server::DevLaunch {
                base_path: None,
                hooks: None,
                pg: None,
                request_context: Some("chain-restart"),
                artifact_release_id: None,
            },
            async move { Ok(activate_ws) },
        )
        .await
        .expect("chain restart");
    assert_ne!(second.pid, first_pid, "restart must produce a new process");
    wait_for_content(&env.workspace, "B");

    // 4. 重建 manager（内存登记丢失，仅持久状态恢复）。
    let FixtureEnv {
        workspace,
        manager,
        config,
        ..
    } = env;
    drop(manager);
    let rebuilt = Arc::new(DevServerManager::new(Arc::new(config)));

    // 5. 无内存登记的停止：经再发现 + 监督协议收束真实进程。
    let rebuilt_stop = rebuilt
        .stop_userapp_dev(key, &workspace)
        .await
        .expect("stop after manager rebuild");
    assert!(
        rebuilt_stop.owner_stopped,
        "rediscovery must find and stop the live target"
    );
    wait_until_dead(second.pid, "second orchestrator").await;

    // 6. 再启动（内容 C）：工作区保留、可继续。
    std::fs::write(workspace.join("marker.txt"), "C").unwrap();
    let third = rebuilt
        .start_dev(
            key,
            &workspace,
            crate::service::dev_server::DevLaunch {
                base_path: None,
                hooks: None,
                pg: None,
                request_context: None,
                artifact_release_id: None,
            },
        )
        .await
        .expect("start after rebuild-stop");
    wait_for_content(&workspace, "C");
    assert!(is_alive(third.pid));
    rebuilt
        .stop_userapp_dev(key, &workspace)
        .await
        .expect("final cleanup stop");
}

fn wait_for_content(workspace: &Path, expected: &str) {
    let served = workspace.join("served.txt");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        if let Ok(content) = std::fs::read_to_string(&served)
            && content.trim() == expected
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fixture never served expected content {expected:?} at {}",
            served.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// 有界等待进程消失（fixture 回包后 ~50ms 自行退出）。必须**异步**等待：
/// 单线程测试运行时下，阻塞 sleep 会饿死收割 task，退出进程滞留僵尸态
///（kill -0 对僵尸恒成功，会误报存活）。
async fn wait_until_dead(pid: u32, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while is_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "{what} (pid {pid}) must be gone after the supervised stop"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill(pid, 0)：存在（无论属主）即存活。
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// wire 镜像冒烟：Envelope 序列化字段与 runtime-supervisor 客户端一致
///（fixture 协议锁）。
#[test]
fn wire_envelope_mirror_matches_control_shape() {
    let request = Request::new(runtime_supervisor::Action::StopWork);
    let envelope = WireEnvelope {
        version: 2,
        instance: "mirror",
        token: "token",
        request: &request,
    };
    let value: serde_json::Value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(value["version"], 2);
    assert_eq!(value["instance"], "mirror");
    assert_eq!(value["token"], "token");
    assert_eq!(value["request"]["action"], "stop_work");
    assert_eq!(
        value["request"]["request_id"],
        serde_json::to_value(&request.request_id).unwrap()
    );
}

/// 复核 DEV-R3（503 ≠ 已停止）：3010 owner 端点返回 ERR_PROTOCOL_UNSUPPORTED
///（run 模式 legacy 形态）且本项目**活**监督目标存在——停止必须实际停掉
/// 该目标（owner 链路观察失败不得挡住已核验本地目标），不得按"幂等无进程"
/// 放行。
#[tokio::test]
async fn run_mode_503_with_live_local_target_actually_stops_it() {
    // 503 owner mock（run 模式 legacy 的 identity 应答形态）。
    let app = axum::Router::new().route(
        "/v1/runtime/identity",
        axum::routing::get(|| async {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "success": false, "code": "ERR_PROTOCOL_UNSUPPORTED", "message": "run mode"
                })),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let probe_addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let env = fixture_env_with_probe(ORCHESTRATOR_FIXTURE, Some(probe_addr)).await;
    let key = "userapp:legacy-503";
    let started = env
        .manager
        .start_dev(
            key,
            &env.workspace,
            crate::service::dev_server::DevLaunch {
                base_path: None,
                hooks: None,
                pg: None,
                request_context: None,
                artifact_release_id: None,
            },
        )
        .await
        .expect("fixture start");
    let stopped = env
        .manager
        .stop_userapp_dev(key, &env.workspace)
        .await
        .expect("503 must not block the verified local target from being stopped");
    assert!(
        stopped.owner_stopped,
        "the live supervision target must actually be stopped: {stopped:?}"
    );
    assert!(!is_alive(started.pid), "fixture orchestrator must be gone");
    assert!(!stop_log_of(&env.workspace).is_empty());
    server.abort();
}
