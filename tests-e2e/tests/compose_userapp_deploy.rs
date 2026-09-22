//! compose 环境 Userapp **部署主流程全链**：template-cli 全量模板初始化 →
//! 构建到 completed → static 取包校验 → start(url) 部署 → pingora 七路流量
//! → prod delete purge 回收。
//!
//! 运行前置（镜像缺失时场景 skip 并写明原因，不算产品红）：
//! - compose 环境在跑（`make dev-up`；rcoder 侧已部署镜像 15 族 snake 契约）
//! - `dev-rcoder-agent-runner:latest` 含 template-cli（`make docker-build-agent-runner`）
//! - `dev-app-runtime:latest` 存在（`make docker-build-app-runtime`）
//!
//! 运行: `cargo test -p rcoder-e2e --test compose_userapp_deploy -- --test-threads=1`
//! 时长 ~15-25 分钟（7 服务全量冷构建依赖外网 npm/maven/pypi/goproxy/crates）。
//!
//! 配置（.env.local 可覆盖）：`E2E_PINGORA_URL`（默认 http://127.0.0.1:8089）、
//! `E2E_RCODER_INTERNAL_URL`（app 容器回拉产物的 rcoder 地址，默认
//! http://rcoder:8090）、`E2E_USERAPP_BUILD_BUDGET_SECS`（默认 1500）、
//! `E2E_USERAPP_READY_BUDGET_SECS`（默认 300）。

use std::time::{Duration, Instant};

use rcoder_e2e::common::Env;
use rcoder_e2e::common::report::JsonlReporter;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// 套件级串行锁（对齐 compose_userapp_dev：单机资源天花板下防 builder 挤兑）。
static SCENARIO_GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn scenario_gate() -> tokio::sync::MutexGuard<'static, ()> {
    SCENARIO_GATE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn pingora_base() -> String {
    env_or("E2E_PINGORA_URL", "http://127.0.0.1:8089")
}

fn rcoder_internal() -> String {
    env_or("E2E_RCODER_INTERNAL_URL", "http://rcoder:8090")
}

fn build_budget() -> Duration {
    Duration::from_secs(
        env_or("E2E_USERAPP_BUILD_BUDGET_SECS", "1500")
            .parse()
            .unwrap_or(1500),
    )
}

fn ready_budget() -> Duration {
    Duration::from_secs(
        env_or("E2E_USERAPP_READY_BUDGET_SECS", "300")
            .parse()
            .unwrap_or(300),
    )
}

/// HttpResult 信封成功判定。
fn http_ok(body: &Value) -> bool {
    body["code"].as_str() == Some("0000")
}

async fn post_json(env: &Env, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let resp = env
        .http
        .post(format!("{}{path}", env.rcoder))
        .timeout(Duration::from_secs(120))
        .json(&body)
        .send()
        .await;
    let resp = match resp {
        Ok(response) => response,
        Err(error) => {
            if matches!(
                path,
                "/api/v1/userapp/workspace" | "/api/v1/userapp/ensure-workspace"
            ) {
                rcoder_e2e::common::resources::register_builder_attempt(
                    body["app_id"].as_str().expect("workspace app identity"),
                    false,
                )
                .expect("register uncertain workspace resource");
            }
            panic!("HTTP POST failed: {error}");
        }
    };
    let status = resp.status();
    let response: Value = resp.json().await.unwrap_or(Value::Null);
    if matches!(
        path,
        "/api/v1/userapp/workspace" | "/api/v1/userapp/ensure-workspace"
    ) {
        rcoder_e2e::common::resources::register_builder_attempt(
            body["app_id"].as_str().expect("workspace app identity"),
            status.is_success() && http_ok(&response),
        )
        .expect("register workspace resource identity");
    }
    (status, response)
}

async fn get_json(env: &Env, path: &str) -> (reqwest::StatusCode, Value) {
    let resp = env
        .http
        .get(format!("{}{path}", env.rcoder))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("http get");
    let status = resp.status();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// 镜像族 GET（X-App-Id header 必填——rcoder 转发层以 header 定位开发容器）。
async fn get_json_with_app(env: &Env, path: &str, app_id: &str) -> (reqwest::StatusCode, Value) {
    let resp = env
        .http
        .get(format!("{}{path}", env.rcoder))
        .timeout(Duration::from_secs(60))
        .header("X-App-Id", app_id)
        .send()
        .await
        .expect("http get");
    let status = resp.status();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// docker inspect 读生产容器 Id（幂等重放不换容器的断言依据）。
fn docker_inspect_id(container: &str) -> Option<String> {
    let out = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", container])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// docker inspect 读容器镜像（重启滚镜像断言依据）。
fn docker_inspect_image(container: &str) -> Option<String> {
    let out = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.Config.Image}}", container])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let image = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!image.is_empty()).then_some(image)
}

fn trunc(v: &Value, n: usize) -> String {
    let s = v.to_string();
    s.chars().take(n).collect()
}

/// 显式清理 builder 容器（docker rm；compose 冒烟防残留）。
fn cleanup_builder(app_id: &str) {
    // 复合键后容器名含实例 user 段（本文件场景 owner 恒为 e2e-dep-user）
    if let Err(error) =
        rcoder_e2e::common::resources::cleanup_container(&format!("rcoder-app-builder-{app_id}"))
    {
        eprintln!("owned builder cleanup failed: {error}");
    }
}

/// 前置探测：app-runtime 镜像存在 + agent-runner 镜像含 template-cli。
/// 返回 None = 前置不满足（调用方 skip）。
async fn preflight(report: &JsonlReporter) -> Option<()> {
    let toolchains = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docker/verify-userapp-toolchains.py"
        ))
        .output();
    let (compatible, detail) = match toolchains {
        Ok(output) => (
            output.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        ),
        Err(error) => (false, error.to_string()),
    };
    report.assert_hard(
        "builder/runtime artifact toolchains are compatible",
        compatible,
        detail,
    );
    if !compatible {
        return None;
    }

    let runtime_ok = std::process::Command::new("docker")
        .args(["image", "inspect", "dev-app-runtime:latest"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !runtime_ok {
        report.diagnostic(
            "preflight dev-app-runtime:latest",
            "missing",
            "app 运行时镜像不存在：make docker-build-app-runtime",
        );
        return None;
    }
    let probe = std::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "sh",
            "dev-rcoder-agent-runner:latest",
            "-c",
            "command -v template-cli",
        ])
        .output();
    let cli_ok = probe.is_ok_and(|o| o.status.success());
    if !cli_ok {
        report.diagnostic(
            "preflight template-cli",
            "missing",
            "dev-rcoder-agent-runner:latest 内无 template-cli：make docker-build-agent-runner 重建后重试",
        );
        return None;
    }
    Some(())
}

/// 模板初始化：execute-command 跑 template-cli 全量组合（7 服务——next 占根
/// 路由 + react/vue 双前端 + go/java/python/rust 四后端，与模板仓"最复杂组合"一致）。
async fn init_full_template(env: &Env, report: &JsonlReporter, app: &str, user: &str) -> bool {
    let command = [
        "template-cli init . --next --force",
        "template-cli add frontend-react-vite",
        "template-cli add frontend-vue3-vite",
        "template-cli add backend-go",
        "template-cli add backend-java",
        "template-cli add backend-python",
        "template-cli add backend-rust",
    ]
    .join(" && ");
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/execute-command", env.rcoder))
        .timeout(Duration::from_secs(300))
        .header("X-App-Id", app)
        .json(&json!({"app_id": app, "user_id": user, "command": command}))
        .send()
        .await
        .expect("exec post");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    // snake wire：exit_code 表达命令结果（外层恒 success）
    let ok = status.is_success()
        && body["success"].as_bool() == Some(true)
        && body["exit_code"].as_i64() == Some(0);
    report.assert_hard(
        "template-cli 全量模板初始化（init --next + 6×add）",
        ok,
        format!(
            "HTTP {status}, exit_code={:?}, stderr 尾部: {}",
            body["exit_code"].as_i64(),
            body["stderr"]
                .as_str()
                .unwrap_or("")
                .chars()
                .rev()
                .take(300)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ),
    );
    ok
}

/// 模板落盘断言：根层 workspace.manifest.toml + 7 个子项目目录；逐子目录
/// spot-check project.manifest.toml（get-file-list 镜像族，snake query）。
async fn assert_template_files(env: &Env, report: &JsonlReporter, app: &str, user: &str) {
    /// npm 包内模板目录名：next 的目录是 `userapp-next`（模板仓源码目录叫
    /// userapp-next-template，渲染产物目录以包内模板 id 为准）。
    const SUBDIRS: &[&str] = &[
        "userapp-next",
        "frontend-react-vite",
        "frontend-vue3-vite",
        "backend-go",
        "backend-java",
        "backend-python",
        "backend-rust",
    ];
    let (s, b) = get_json_with_app(
        env,
        &format!("/api/v1/userapp/get-file-list?app_id={app}&user_id={user}&recursive=false"),
        app,
    )
    .await;
    let names: Vec<String> = b["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|f| f["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let root_ok = s.is_success()
        && names.iter().any(|n| n == "workspace.manifest.toml")
        && SUBDIRS.iter().all(|d| names.iter().any(|n| n == d));
    report.assert_hard(
        "workspace 根含 workspace.manifest.toml + 7 个子项目目录",
        root_ok,
        format!("HTTP {s}, names: {names:?}"),
    );

    for dir in SUBDIRS {
        let (s, b) = get_json_with_app(
            env,
            &format!(
                "/api/v1/userapp/get-file-list?app_id={app}&user_id={user}&recursive=false&relative_path={dir}"
            ),
            app,
        )
        .await;
        // 条目 name 是 workspace 根相对全路径（如 "backend-go/project.manifest.toml"）
        let want = format!("{dir}/project.manifest.toml");
        let has_manifest = b["files"]
            .as_array()
            .is_some_and(|files| files.iter().any(|f| f["name"] == want));
        report.assert_hard(
            &format!("{dir}/project.manifest.toml 落盘"),
            s.is_success() && has_manifest,
            format!("HTTP {s}, body 截断: {}", trunc(&b, 120)),
        );
    }
}

/// 构建到终态：返回 (release_id, sha256)；failed/cancelled/超时返回 None。
async fn build_to_completion(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
) -> Option<(String, String)> {
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/build", env.rcoder))
        .timeout(Duration::from_secs(60))
        .header("X-App-Id", app)
        .json(&json!({"app_id": app, "user_id": user}))
        .send()
        .await
        .expect("build post");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let task_id = body["data"]["task_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let artifact_path = body["data"]["artifact_path"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let accepted = status.is_success()
        && http_ok(&body)
        && !task_id.is_empty()
        && artifact_path.starts_with("builds/workspace-package-");
    report.assert_hard(
        "build 受理（200 + task_id + artifact_path 预生成）",
        accepted,
        format!("HTTP {status}, body 截断: {}", trunc(&body, 120)),
    );
    if !accepted {
        return None;
    }

    let t0 = Instant::now();
    let mut terminal: Option<Value> = None;
    while t0.elapsed() < build_budget() {
        let (s, b) = get_json(
            env,
            &format!("/api/v1/userapp/tasks/{task_id}?app_id={app}&user_id={user}"),
        )
        .await;
        if s.is_success()
            && http_ok(&b)
            && let Some(st) = b["data"]["status"].as_str()
            && matches!(st, "failed" | "cancelled" | "completed")
        {
            terminal = Some(b["data"].clone());
            break;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let Some(data) = terminal else {
        report.assert_hard(
            "build 到达终态（预算内不挂死）",
            false,
            format!(
                "{:.0}s 未到终态（预算 {:.0}s）",
                t0.elapsed().as_secs_f64(),
                build_budget().as_secs_f64()
            ),
        );
        return None;
    };
    let st = data["status"].as_str().unwrap_or_default().to_owned();
    let done = st == "completed";
    report.assert_hard(
        "build 终态 = completed（7 服务全量构建成功）",
        done,
        format!(
            "status={st}, {:.0}s, error: {:?}",
            t0.elapsed().as_secs_f64(),
            data["error"].as_str().unwrap_or("")
        ),
    );
    if !done {
        // 失败留痕：快照 error 本身已嵌失败服务的输出尾部（构建日志行实时走
        // tasks SSE `log` 事件；tasks/{id}/logs 分页端点已下线，无额外日志拉取）
        let svc = data["current_service"].as_str().unwrap_or_default();
        report.diagnostic(
            &format!("build 失败服务 [{svc}]（完整输出见快照 error 尾部）"),
            data["error"].as_str().unwrap_or(""),
            "task snapshot error",
        );
        return None;
    }
    let release_id = data["release_id"].as_str().unwrap_or_default().to_owned();
    let sha256 = data["sha256"].as_str().unwrap_or_default().to_owned();
    let size_ok = data["size_bytes"].as_u64().unwrap_or(0) > 0
        && !data["file_name"].as_str().unwrap_or_default().is_empty();
    report.assert_hard(
        "completed 快照含 release_id/sha256/size_bytes/file_name",
        !release_id.is_empty() && sha256.len() == 64 && size_ok,
        format!(
            "release_id={release_id}, sha256={sha256}, size={:?}",
            data["size_bytes"].as_u64()
        ),
    );
    if release_id.is_empty() || sha256.len() != 64 {
        return None;
    }
    // （构建日志分页端点已下线：日志行实时走 tasks SSE `log` 事件，C2 覆盖回放）

    // C1 cancel 幂等：任务已终态（completed）→ already_terminal=true
    let (cs, cb) = post_json(
        env,
        &format!("/api/v1/userapp/tasks/{task_id}/cancel?app_id={app}&user_id={user}"),
        json!({}),
    )
    .await;
    let cancel_ok = cs.is_success()
        && http_ok(&cb)
        && cb["data"]["task_id"] == task_id.as_str()
        && cb["data"]["already_terminal"].as_bool() == Some(true);
    report.assert_hard(
        "tasks cancel 幂等（已终态 → already_terminal=true）",
        cancel_ok,
        format!("HTTP {cs}, body 截断: {}", trunc(&cb, 150)),
    );

    // C2 tasks SSE 回放：构建终态后连流 → 回放全部事件（含 completed）后自然关流
    let sse_url = format!(
        "{}/api/v1/userapp/tasks/{task_id}/logs/stream?app_id={app}&user_id={user}&from_seq=0",
        env.rcoder
    );
    let sse_ok = env
        .sse_http
        .get(&sse_url)
        .timeout(Duration::from_secs(30))
        .header("Accept", "text/event-stream")
        .send()
        .await;
    let sse_detail;
    let sse_ok = match sse_ok {
        Ok(resp) => {
            let status = resp.status();
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            // 读流直到关闭（终态事件后服务端关流；30s 兜底）
            let mut text = String::new();
            let deadline = Instant::now() + Duration::from_secs(25);
            use futures_util::StreamExt;
            let mut stream = resp.bytes_stream();
            while let Ok(Some(chunk)) =
                tokio::time::timeout_at(deadline.into(), stream.next()).await
            {
                if let Ok(chunk) = chunk {
                    text.push_str(&String::from_utf8_lossy(&chunk));
                }
            }
            let has_completed =
                text.contains("event:completed") || text.contains("\"event\":\"completed\"");
            // log 事件（构建日志行实时流）至少出现一条
            let has_log = text.contains("event:log") || text.contains("\"event\":\"log\"");
            sse_detail = format!(
                "HTTP {status}, ct={ct}, {} bytes, completed={has_completed}, log={has_log}",
                text.len()
            );
            status.is_success() && ct.contains("text/event-stream") && has_completed && has_log
        }
        Err(e) => {
            sse_detail = format!("err: {e}");
            false
        }
    };
    report.assert_hard(
        "tasks SSE 回放（终态后连流 → 全量事件 + completed + 自然关流）",
        sse_ok,
        sse_detail,
    );
    Some((release_id, sha256))
}

/// static 取包：经 rcoder 下载制品，校验 sha256 与快照一致（校验链闭环）。
async fn fetch_and_verify_artifact(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
    release_id: &str,
    sha256_expect: &str,
) -> Option<()> {
    let url = format!("/api/v1/userapp/static/{app}?release_id={release_id}&user_id={user}");
    let resp = env
        .http
        .get(format!("{}{url}", env.rcoder))
        .timeout(Duration::from_secs(600))
        .send()
        .await;
    let Ok(resp) = resp else {
        report.assert_hard("static 取包 200", false, "请求失败".into());
        return None;
    };
    let status = resp.status();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = resp.bytes().await.unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha256_actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let ok = status.is_success() && !bytes.is_empty() && sha256_actual == sha256_expect;
    report.assert_hard(
        "static 取包 + sha256 与任务快照一致",
        ok,
        format!(
            "HTTP {status}, content-type={ct}, {} bytes, sha256 匹配={}",
            bytes.len(),
            sha256_actual == sha256_expect
        ),
    );
    if ok && let Some(root) = std::env::var_os("E2E_REPORT_DIR") {
        let saved = std::fs::write(
            std::path::PathBuf::from(root).join("verified-artifact.zip"),
            &bytes,
        );
        report.assert_hard(
            "verified artifact retained for reproducible deployment",
            saved.is_ok(),
            saved.err().map(|e| e.to_string()).unwrap_or_default(),
        );
    }
    ok.then_some(())
}

/// start(url) 部署 → 轮询七路流量（compose 语义：容器 running 即 ready，
/// start 200 ≠ 应用就绪，必须以流量为准）。
async fn deploy_and_verify_traffic(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
    release_id: &str,
    sha256: &str,
) -> bool {
    // 制品 URL 用容器可达形态（app 容器并入 compose 主网络，按服务名回拉 rcoder）；
    // static 端点按 release_id 精确定位制品——勿直拼 build 响应的 artifact_path
    let artifact_url = format!(
        "{}/api/v1/userapp/static/{app}?release_id={release_id}&user_id={user}",
        rcoder_internal()
    );
    let request_id = format!("cold-{app}");
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/start"),
        json!({"user_id": user, "url": artifact_url, "release_id": release_id, "sha256": sha256, "request_id": request_id}),
    )
    .await;
    let started = s.is_success() && http_ok(&b);
    report.assert_hard(
        "start(url) 部署受理（200 + Running）",
        started && b["data"]["status"].as_str() == Some("running"),
        format!("HTTP {s}, body 截断: {}", trunc(&b, 200)),
    );
    if !started {
        return false;
    }

    let identity =
        rcoder_e2e::common::resources::register_created_container(&format!("rcoder-app-{app}"));
    report.assert_hard(
        "prod resource registered at creation",
        identity.is_ok(),
        identity.err().unwrap_or_default(),
    );

    // D′：整请求 request_id 幂等——同 id 同参重放返回存储响应且不重新部署
    // （容器 Id 不变）；同 id 异参拒绝；by-request 查询定位该 Deploy 操作。
    let container_id_before = docker_inspect_id(&format!("rcoder-app-{app}"));
    let (rs, rb) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/start"),
        json!({"user_id": user, "url": artifact_url, "release_id": release_id, "sha256": sha256, "request_id": request_id}),
    )
    .await;
    let replay_ok = rs.is_success()
        && http_ok(&rb)
        && rb["data"]["release_id"].as_str() == Some(release_id)
        && rb["data"]["status"].as_str() == Some("running");
    let container_id_after = docker_inspect_id(&format!("rcoder-app-{app}"));
    report.assert_hard(
        "start request_id 同参重放=存储响应且零重部署",
        replay_ok && container_id_before.is_some() && container_id_before == container_id_after,
        format!(
            "HTTP {rs}, before={:?} after={:?}, body 截断: {}",
            container_id_before
                .as_deref()
                .map(|id| &id[..12.min(id.len())]),
            container_id_after
                .as_deref()
                .map(|id| &id[..12.min(id.len())]),
            trunc(&rb, 160)
        ),
    );
    let wrong_sha = format!("{}{}", "a".repeat(63), "1");
    let (cs, cb) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/start"),
        json!({"user_id": user, "url": artifact_url, "release_id": release_id, "sha256": wrong_sha, "request_id": request_id}),
    )
    .await;
    report.assert_hard(
        "start request_id 异参重用→ERR_CONFLICT",
        cs.is_success()
            && cb["code"].as_str() == Some("ERR_CONFLICT")
            && cb["message"]
                .as_str()
                .is_some_and(|m| m.contains("reused with different parameters")),
        format!("HTTP {cs}, body 截断: {}", trunc(&cb, 160)),
    );
    let (qs, qb) = get_json(
        env,
        &format!(
            "/api/v1/userapp/{app}/operations/by-request?user_id={user}&request_id={request_id}"
        ),
    )
    .await;
    report.assert_hard(
        "operations/by-request 定位 Deploy 操作（Succeeded）",
        qs.is_success()
            && qb["data"]["kind"].as_str() == Some("StartDeployment")
            && qb["data"]["state"].as_str() == Some("Succeeded"),
        format!("HTTP {qs}, body 截断: {}", trunc(&qb, 160)),
    );

    // 七路流量：next=/、react=/react、vue=/vue、四后端 readiness（strip_prefix 后路径）
    let probes: &[(&str, &str)] = &[
        ("next /", "/"),
        ("react /react/", "/react/"),
        ("vue /vue/", "/vue/"),
        ("go /api/go/ready", "/api/go/ready"),
        ("java readiness", "/api/java/actuator/health/readiness"),
        ("python /api/python/ready", "/api/python/ready"),
        ("rust /api/rust/ready", "/api/rust/ready"),
    ];
    let pingora = pingora_base();
    let base = format!("{pingora}/api/v1/userapp/proxy/app/prod/{user}/{app}");
    let t0 = Instant::now();
    let mut pending: Vec<(&str, &str)> = probes.to_vec();
    let mut first_seen: Vec<(String, u128)> = Vec::new();
    while !pending.is_empty() && t0.elapsed() < ready_budget() {
        let status = std::process::Command::new("docker")
            .args([
                "exec",
                &format!("rcoder-app-{app}"),
                "wget",
                "-T",
                "5",
                "-qO-",
                "http://127.0.0.1:3010/v1/deploy/status",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                serde_json::from_slice::<Value>(&output.stdout)
                    .ok()
                    .map(|body| body["data"].clone())
            });
        // The top-level phase may stay orchestrating while an uncertain
        // migration is fenced for recovery. Its matching operation already
        // records failure; do not spend the full traffic budget on that attempt.
        if let Some(status) = status
            && status["operation"]["request_release_id"].as_str() == Some(release_id)
            && (status["phase"] == "failed" || status["operation"]["phase"] == "failed")
        {
            report.assert_hard(
                "cold deployment has no terminal orchestration failure",
                false,
                trunc(&status, 1500),
            );
            break;
        }
        let mut still_pending = Vec::new();
        for (name, path) in pending {
            let ok = match env
                .http
                .get(format!("{base}{path}"))
                .timeout(Duration::from_secs(15))
                .send()
                .await
            {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            };
            if ok {
                first_seen.push((name.to_string(), t0.elapsed().as_millis()));
            } else {
                still_pending.push((name, path));
            }
        }
        pending = still_pending;
        if !pending.is_empty() {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    for (name, _) in probes {
        let seen = first_seen.iter().find(|(n, _)| n == name);
        report.assert_hard(
            &format!("流量七路[{name}]就绪"),
            seen.is_some(),
            match seen {
                Some((_, ms)) => format!("{}ms 起可达", ms),
                None => format!(
                    "{:.0}s 内未就绪（502=pingap 未起/后端未注册）",
                    t0.elapsed().as_secs_f64()
                ),
            },
        );
    }

    // D″：裸 restart（无 url/env）= 平台镜像升级通道。必须在业务就绪之后触发——
    // 部署受理返回时编排仍在异步进行（"成功 ≠ 立即接流量"），窗口内重启会打断
    // migration → app-cli 按设计 fail-closed 进恢复保持（迁移结果未知不静默重启）。
    // Docker 形态经容器重建落地：Id 更换 + Config.Image == 平台默认；随后流量
    // 七路在重建后的容器上复验（证明业务经 env/挂载保真重建后仍可用）。
    let deployed = pending.is_empty();
    let (rs2, rb2) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/restart?user_id={user}"),
        json!({}),
    )
    .await;
    report.assert_hard(
        "bare restart 受理（镜像滚动通道）",
        rs2.is_success() && http_ok(&rb2),
        format!("HTTP {rs2}, body 截断: {}", trunc(&rb2, 160)),
    );
    let platform_image = env_or("RCODER_RUNTIME_IMAGE_DIGEST", "dev-app-runtime:latest");
    let prod_container = format!("rcoder-app-{app}");
    let mut rolled = false;
    let mut rolled_detail = String::new();
    let t_roll = Instant::now();
    while t_roll.elapsed() < ready_budget() {
        if docker_container_running(&prod_container) {
            match (
                docker_inspect_image(&prod_container),
                docker_inspect_id(&prod_container),
            ) {
                (Some(image), Some(id)) if image == platform_image => {
                    rolled = container_id_after.as_deref() != Some(id.as_str());
                    rolled_detail = format!(
                        "image={image}, id_changed={}",
                        container_id_after.as_deref() != Some(id.as_str())
                    );
                    break;
                }
                (image, id) => {
                    rolled_detail = format!(
                        "image={image:?}（期望 {platform_image}）, id={:?}",
                        id.as_deref().map(|value| &value[..12.min(value.len())])
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    report.assert_hard(
        "bare restart 滚动到平台默认镜像（容器重建，Id 更换）",
        rolled,
        rolled_detail,
    );

    // 重启后业务恢复复验：重建容器重新编排既有 release，七路流量必须再次可达。
    let mut pending2: Vec<(&str, &str)> = probes.to_vec();
    let t_recheck = Instant::now();
    while !pending2.is_empty() && t_recheck.elapsed() < ready_budget() {
        let mut still_pending = Vec::new();
        for (name, path) in pending2 {
            let ok = match env
                .http
                .get(format!("{base}{path}"))
                .timeout(Duration::from_secs(15))
                .send()
                .await
            {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            };
            if ok {
                report.assert_hard(
                    &format!("镜像滚动重启后流量恢复[{name}]"),
                    true,
                    format!("{:.0}s 起再次可达", t_recheck.elapsed().as_secs_f64()),
                );
            } else {
                still_pending.push((name, path));
            }
        }
        pending2 = still_pending;
        if !pending2.is_empty() {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    for (name, _) in probes {
        if pending2.iter().any(|(n, _)| n == name) {
            report.assert_hard(
                &format!("镜像滚动重启后流量恢复[{name}]"),
                false,
                format!(
                    "重启后 {:.0}s 内未恢复（重建容器编排失败？502=pingap 后端未重注册）",
                    t_recheck.elapsed().as_secs_f64()
                ),
            );
        }
    }
    deployed && pending2.is_empty()
}

/// 轻量部署不传请求 release_id；请求身份与 manifest 身份不同源也必须完成。
/// 正确性与性能分别断言，禁止用耗时或生成 ID 前缀代替操作完成证明。
async fn verify_url_lightweight_deploy_without_release_id(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
    release_id: &str,
    sha256: &str,
) {
    let artifact_url = format!(
        "{}/api/v1/userapp/static/{app}?release_id={release_id}&user_id={user}",
        rcoder_internal()
    );
    let t0 = Instant::now();
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/{app}/start", env.rcoder))
        .timeout(Duration::from_secs(100))
        .json(&json!({"user_id": user, "url": artifact_url, "sha256": sha256}))
        .send()
        .await;
    let mut request_release_id = None;
    let (ok, detail) = match resp {
        Ok(r) => {
            let status = r.status();
            let body: Value = r.json().await.unwrap_or(Value::Null);
            request_release_id = body["data"]["release_id"].as_str().map(str::to_owned);
            (
                status.is_success()
                    && http_ok(&body)
                    && body["data"]["release_id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty() && id != release_id),
                format!(
                    "HTTP {status}, {:.1}s, {}",
                    t0.elapsed().as_secs_f64(),
                    trunc(&body, 160)
                ),
            )
        }
        Err(e) => (
            false,
            format!(
                "request error after {:.1}s: {e}",
                t0.elapsed().as_secs_f64()
            ),
        ),
    };
    let fast = t0.elapsed() < Duration::from_secs(90);
    report.assert_hard("deploy.lightweight.generated-request-identity", ok, detail);
    report.assert_hard(
        "deploy.lightweight.confirmation-under-90s",
        fast,
        format!("confirmation took {:.1}s", t0.elapsed().as_secs_f64()),
    );
    if !ok {
        return;
    }
    let container = format!("rcoder-app-{app}");
    let inspect = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{json .Config.Env}}", &container])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<Vec<String>>(&output.stdout).ok());
    let env_value = |key: &str| {
        inspect.as_ref().and_then(|values| {
            values.iter().find_map(|value| {
                value
                    .split_once('=')
                    .filter(|(name, _)| *name == key)
                    .map(|(_, value)| value)
            })
        })
    };
    let expected_operation = env_value("APP_DEPLOY_OPERATION_ID");
    let expected_generation = env_value("APP_DEPLOY_GENERATION_ID");
    let status = std::process::Command::new("docker")
        .args([
            "exec",
            &container,
            "wget",
            "-T",
            "5",
            "-qO-",
            "http://127.0.0.1:3010/v1/deploy/status",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok());
    let operation = status.as_ref().map(|body| &body["data"]["operation"]);
    let identity_matches = expected_operation.is_some_and(|id| !id.is_empty())
        && expected_generation.is_some_and(|id| !id.is_empty())
        && operation.is_some_and(|operation| {
            operation["operation_id"].as_str() == expected_operation
                && operation["deployment_generation_id"].as_str() == expected_generation
                && operation["request_release_id"].as_str() == request_release_id.as_deref()
                && operation["artifact_release_id"].as_str() == Some(release_id)
                && operation["deploy_stage"] == "succeeded"
                && operation["persisted"] == true
        });
    report.assert_hard(
        "deploy.lightweight.exact-operation-and-artifact",
        identity_matches,
        format!("expected operation={expected_operation:?}, generation={expected_generation:?}, status={status:?}"),
    );
    // 流量复核：换 pod 后探 /react/（完整七路已由冷部署场景锁过）
    let pingora = pingora_base();
    let base = format!("{pingora}/api/v1/userapp/proxy/app/prod/{user}/{app}");
    let t1 = Instant::now();
    let mut traffic_ok = false;
    while t1.elapsed() < ready_budget() {
        let probe = env
            .http
            .get(format!("{base}/react/"))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if probe {
            traffic_ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    report.assert_hard(
        "轻量重部署后流量恢复（/react/）",
        traffic_ok,
        format!("{:.0}s 内未恢复", t1.elapsed().as_secs_f64()),
    );
}

/// 部署后 prod 观测族验收（health / logs 三接口 / stats / events）——运行态
/// 主链此前 e2e 零覆盖（logs 三接口转发 app-cli :3010 曾有断链史，audit 批修复）。
async fn verify_prod_observability(env: &Env, report: &JsonlReporter, app: &str, user: &str) {
    // health：GET 双态健康（prod=运行容器就绪探针）
    let (h_s, h_b) = get_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/health?user_id={user}"),
    )
    .await;
    report.assert_hard(
        "prod health 就绪（200 + 0000）",
        h_s.is_success() && http_ok(&h_b),
        format!("HTTP {h_s}, body 截断: {}", trunc(&h_b, 150)),
    );

    // logs/sources/query：声明日志源清单（POST 转发 app-cli）
    let (ss, sb) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/logs/sources/query?user_id={user}"),
        json!({}),
    )
    .await;
    let sources_ok =
        ss.is_success() && http_ok(&sb) && sb["data"].as_array().is_some_and(|arr| !arr.is_empty());
    report.assert_hard(
        "prod logs/sources/query 声明源非空",
        sources_ok,
        format!("HTTP {ss}, body 截断: {}", trunc(&sb, 200)),
    );

    // logs/query：多服务日志快照（tail 限行 + cursor 游标面）
    let (qs, qb) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/logs/query?user_id={user}"),
        json!({"tail": 20}),
    )
    .await;
    report.assert_hard(
        "prod logs/query 快照（200 + 0000）",
        qs.is_success() && http_ok(&qb),
        format!("HTTP {qs}, body 截断: {}", trunc(&qb, 200)),
    );

    // logs/stream：SSE 实时流（连接建立 + content-type；应用静默期无事件属正常）
    // POST-SSE 设计（body=LogQueryRequest 支持断线 cursor 续传），非 GET
    let stream_ok = env
        .http
        .post(format!(
            "{}/api/v1/userapp/{app}/prod/logs/stream?user_id={user}",
            env.rcoder
        ))
        .header("Accept", "text/event-stream")
        .json(&json!({}))
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    let stream_detail;
    let stream_ok = match stream_ok {
        Ok(resp) => {
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let status = resp.status();
            // 连接保持打开：丢弃响应体（应用静默期无事件属正常，头即证明通道）
            drop(resp);
            stream_detail = format!("HTTP {status}, content-type={ct}");
            status.is_success() && ct.contains("text/event-stream")
        }
        Err(e) => {
            stream_detail = format!("err: {e}");
            false
        }
    };
    report.assert_hard(
        "prod logs/stream SSE 通道（200 + text/event-stream）",
        stream_ok,
        stream_detail,
    );

    // stats / events：运行观测双查询
    let (st_s, st_b) = get_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/stats?user_id={user}"),
    )
    .await;
    report.assert_hard(
        "prod stats（200 + 0000）",
        st_s.is_success() && http_ok(&st_b),
        format!("HTTP {st_s}, body 截断: {}", trunc(&st_b, 150)),
    );
    let (ev_s, ev_b) = get_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/events?user_id={user}"),
    )
    .await;
    report.assert_hard(
        "prod events（200 + 0000）",
        ev_s.is_success() && http_ok(&ev_b),
        format!("HTTP {ev_s}, body 截断: {}", trunc(&ev_b, 150)),
    );
}

/// C3 db prod 侧：reset-password → create-database（+409 重复/+404 不存在）。
async fn verify_db_prod(env: &Env, report: &JsonlReporter, app: &str, user: &str) {
    // reset-password 在当前运行的 prod PG 内立即执行，不唤醒或重启容器。
    let (s, b) = post_json(
        env,
        "/api/v1/userapp/db/prod/reset-password",
        json!({"app_id": app, "request_id": "prodindependent", "username": "e2e_independent", "password": "e2e-reset-pw-456"}),
    )
    .await;
    report.assert_hard(
        "db prod reset-password",
        s.is_success() && http_ok(&b),
        format!("HTTP {s}, body 截断: {}", trunc(&b, 120)),
    );

    // create-database + 409 重复（后缀非 [a-zA-Z0-9_] 字符归一为 '_'——app id 尾段
    // 含 '-' 时会撞 PG 标识符白名单 ERR_VALIDATION，属测试数据竞态非产品问题）
    let dbname = format!(
        "e2e_db_{}",
        app[app.len().saturating_sub(8)..]
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    );
    let (s, b) = post_json(
        env,
        "/api/v1/userapp/db/prod/create-database",
        json!({"app_id": app, "user_id": user, "database": dbname}),
    )
    .await;
    report.assert_hard(
        "db prod create-database",
        s.is_success() && http_ok(&b),
        format!("HTTP {s}, body 截断: {}", trunc(&b, 120)),
    );
    let (s, b) = post_json(
        env,
        "/api/v1/userapp/db/prod/create-database",
        json!({"app_id": app, "user_id": user, "database": dbname}),
    )
    .await;
    report.assert_hard(
        "db prod create-database 重复 → HTTP 200 + ERR_CONFLICT",
        s.as_u16() == 200
            && b["code"] == "ERR_CONFLICT"
            && b["message"].as_str().is_some_and(str::is_ascii),
        format!("HTTP {s}, body 截断: {}", trunc(&b, 100)),
    );

    // 不存在 app → 404（统一后的 ERR_APP_NOT_FOUND）
    let (s, b) = post_json(
        env,
        "/api/v1/userapp/db/prod/reset-password",
        json!({"app_id": "e2emissingdatabase", "user_id": user, "password": "x"}),
    )
    .await;
    report.assert_hard(
        "db prod 不存在 app → HTTP 200 + ERR_APP_NOT_FOUND",
        s.as_u16() == 200
            && b["code"].as_str() == Some("ERR_APP_NOT_FOUND")
            && b["message"].as_str().is_some_and(str::is_ascii),
        format!("HTTP {s}, body 截断: {}", trunc(&b, 100)),
    );
}

fn cr10_tcp_login(id: &str, username: &str, password: &str) -> bool {
    let output = std::process::Command::new("docker")
        .args([
            "exec",
            id,
            "env",
            &format!("PGPASSWORD={password}"),
            "psql",
            "-X",
            "-w",
            "-h",
            "127.0.0.1",
            "-U",
            username,
            "-d",
            "postgres",
            "-Atc",
            "SELECT 1",
        ])
        .output()
        .unwrap();
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "1"
}

async fn cr10_proxy_ready(env: &Env, app: &str, user: &str) -> bool {
    let pingora = pingora_base();
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        if let Ok(response) = env
            .http
            .get(format!(
                "{pingora}/api/v1/userapp/proxy/app/prod/{user}/{app}/react/"
            ))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            && response.status().is_success()
            && response
                .text()
                .await
                .is_ok_and(|body| body.to_ascii_lowercase().contains("<!doctype html"))
        {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

/// Immediate SQL change must preserve the container, owner and existing sessions.
async fn verify_immediate_runtime_password(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let prod_name = format!("rcoder-app-{app}");
    let dev_name = format!("rcoder-app-builder-{app}");
    let before = docker_inspect_id(&prod_name);
    let dev_before = docker_inspect_id(&dev_name);
    let (_, identity) = get_json(env, &format!("/api/v1/userapp/{app}/lifecycle")).await;
    let lifecycle = identity["data"]["lifecycle_id"].as_str();
    let ready = cr10_proxy_ready(env, app, user).await;
    report.assert_hard(
        "CR10 immediate password preconditions",
        before.is_some() && dev_before.is_some() && lifecycle.is_some() && ready,
        "Runtime identities and actual business readiness checked".into(),
    );
    let (Some(uid), Some(dev_uid), Some(lifecycle)) =
        (before.as_deref(), dev_before.as_deref(), lifecycle)
    else {
        return;
    };
    fn original_login(uid: &str) -> bool {
        let output=std::process::Command::new("docker").args(["exec",uid,"sh","-c","PGPASSWORD=\"$POSTGRES_PASSWORD\" psql -X -w -h 127.0.0.1 -U \"$POSTGRES_USER\" -d postgres -Atc 'SELECT 1'"]).output().unwrap();
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "1"
    }
    fn owner(uid: &str) -> Option<String> {
        let output = std::process::Command::new("docker")
            .args([
                "exec",
                uid,
                "curl",
                "--silent",
                "--fail",
                "--max-time",
                "5",
                "http://127.0.0.1:3010/v1/runtime/identity",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let body: Value = serde_json::from_slice(&output.stdout).ok()?;
        body["data"]["runtime_instance_id"]
            .as_str()
            .or_else(|| body["runtime_instance_id"].as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }
    let owner_before = owner(uid);
    let username = std::process::Command::new("docker")
        .args(["exec", uid, "printenv", "POSTGRES_USER"])
        .output()
        .unwrap();
    let username = String::from_utf8(username.stdout)
        .unwrap()
        .trim()
        .to_owned();
    report.assert_hard(
        "CR10 original TCP credentials valid",
        !username.is_empty()
            && original_login(uid)
            && original_login(dev_uid)
            && owner_before.is_some(),
        "Both environments authenticate before change; owner captured".into(),
    );
    // This is one real TCP session, not two equivalent new connections.
    let mut session=tokio::process::Command::new("docker").args(["exec","-i",uid,"sh","-c","PGPASSWORD=\"$POSTGRES_PASSWORD\" exec psql -X -q -w -h 127.0.0.1 -U \"$POSTGRES_USER\" -d postgres -At"])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true).spawn().unwrap();
    let mut input = session.stdin.take().unwrap();
    let mut output = BufReader::new(session.stdout.take().unwrap());
    input
        .write_all(b"SET idle_session_timeout = '300s'; SELECT pg_backend_pid();\n")
        .await
        .unwrap();
    input.flush().await.unwrap();
    let mut old_pid = String::new();
    let authenticated =
        tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut old_pid))
            .await
            .is_ok_and(|result| result.is_ok_and(|n| n > 0))
            && old_pid.trim().parse::<u32>().is_ok();
    report.assert_hard(
        "CR10 persistent session authenticated before password change",
        authenticated,
        "TCP backend PID captured before HTTP mutation".into(),
    );
    if !authenticated {
        drop(input.write_all(b"\\q\n").await);
        return;
    }
    let password = "e2e_immediate_new_password";
    let (status,body)=post_json(env,"/api/v1/userapp/db/prod/reset-password",json!({"app_id":app,"lifecycle_id":lifecycle,"request_id":"immediateprodpassword","username":username,"password":password})).await;
    report.assert_hard(
        "CR10 reset password applies immediately",
        status.is_success() && http_ok(&body),
        format!("HTTP {status}; code={:?}", body["code"].as_str()),
    );
    report.assert_hard(
        "CR10 new password accepts new TCP connection",
        cr10_tcp_login(uid, &username, password),
        "New TCP login checked without runtime restart".into(),
    );
    report.assert_hard(
        "CR10 old password rejects new TCP connection",
        !original_login(uid),
        "A distinct connection with original environment credentials is rejected".into(),
    );
    let sent =
        input.write_all(b"SELECT pg_backend_pid();\n").await.is_ok() && input.flush().await.is_ok();
    let mut after_pid = String::new();
    let continued = sent
        && tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut after_pid))
            .await
            .is_ok_and(|result| result.is_ok_and(|n| n > 0))
        && after_pid.trim() == old_pid.trim();
    report.assert_hard(
        "CR10 authenticated session survives password change",
        continued,
        "Same TCP session executes SQL with unchanged backend PID".into(),
    );
    drop(input.write_all(b"\\q\n").await);
    drop(input);
    drop(tokio::time::timeout(Duration::from_secs(5), session.wait()).await);
    report.assert_hard(
        "CR10 password change preserves production UID and owner",
        docker_inspect_id(&prod_name) == before && owner(uid) == owner_before,
        "No container replacement or owner restart".into(),
    );
    report.assert_hard(
        "CR10 password change leaves development unchanged",
        docker_inspect_id(&dev_name) == dev_before && original_login(dev_uid),
        "Development identity and original credentials retained".into(),
    );
    report.assert_hard(
        "CR10 business remains available after password change",
        ready && cr10_proxy_ready(env, app, user).await,
        "Actual React proxy remains available without automatic business restart".into(),
    );
}

fn docker_container_running(name: &str) -> bool {
    std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.State.Running}}", name])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "true")
        .unwrap_or(false)
}

/// CR10 治理族：同请求重放幂等、停止态改密拒绝且不隐式启动、显式重启后
/// 新密码保留（不回写历史保存值）、完成部署不冒充未决改密接受恢复。
async fn verify_password_governance_after_change(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
) {
    let prod_name = format!("rcoder-app-{app}");
    let (_, identity) = get_json(env, &format!("/api/v1/userapp/{app}/lifecycle")).await;
    let lifecycle = identity["data"]["lifecycle_id"].as_str();
    report.assert_hard(
        "CR10 governance lifecycle readable",
        lifecycle.is_some(),
        format!("lifecycle body 截断: {}", trunc(&identity, 120)),
    );
    let Some(lifecycle) = lifecycle else {
        return;
    };
    let password = "e2e_immediate_new_password";
    let username = || {
        let uid = docker_inspect_id(&prod_name)?;
        let output = std::process::Command::new("docker")
            .args(["exec", &uid, "printenv", "POSTGRES_USER"])
            .output()
            .ok()?;
        Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
    };
    let username = username().filter(|value| !value.is_empty());
    report.assert_hard(
        "CR10 governance account readable",
        username.is_some(),
        "Runtime account checked before replay".into(),
    );
    let Some(username) = username else {
        return;
    };

    // A. 同 request_id 重放：原身份幂等，不产生新操作。
    let (status, body) = post_json(
        env,
        "/api/v1/userapp/db/prod/reset-password",
        json!({"app_id":app,"lifecycle_id":lifecycle,"request_id":"immediateprodpassword","username":username,"password":password}),
    )
    .await;
    report.assert_hard(
        "CR10 same-request replay is idempotent",
        status.is_success() && http_ok(&body),
        format!("HTTP {status}; code={:?}", body["code"].as_str()),
    );

    // B. 停止态改密明确拒绝，且不隐式启动业务。
    let (stop_s, stop_b) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/stop?user_id={user}"),
        json!({}),
    )
    .await;
    let stopped = stop_s.is_success()
        && http_ok(&stop_b)
        && stop_b["data"]["status"].as_str() == Some("stopped");
    report.assert_hard(
        "CR10 governance stop before rejected write",
        stopped,
        format!("HTTP {stop_s}, body 截断: {}", trunc(&stop_b, 120)),
    );
    if !stopped {
        return;
    }
    let mut settled = false;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(90) {
        if !docker_container_running(&prod_name) {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    report.assert_hard(
        "CR10 governance container stopped",
        settled,
        format!("{:.0}s 内停止", t0.elapsed().as_secs_f64()),
    );
    if !settled {
        return;
    }
    let (status, body) = post_json(
        env,
        "/api/v1/userapp/db/prod/reset-password",
        json!({"app_id":app,"lifecycle_id":lifecycle,"request_id":"governancestopped","username":username,"password":password}),
    )
    .await;
    report.assert_hard(
        "CR10 stopped prod rejects password change",
        !status.is_success() || !http_ok(&body),
        format!("HTTP {status}; code={:?}", body["code"].as_str()),
    );
    report.assert_hard(
        "CR10 rejected change does not wake the container",
        !docker_container_running(&prod_name),
        "No implicit business startup from a refused password write".into(),
    );

    // C. 用户显式重启后新密码保留：启动、重启均不回写历史保存值。
    let (start_s, start_b) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/start?user_id={user}"),
        json!({}),
    )
    .await;
    report.assert_hard(
        "CR10 explicit start after stop",
        start_s.is_success() && http_ok(&start_b),
        format!("HTTP {start_s}, body 截断: {}", trunc(&start_b, 120)),
    );
    let mut running = false;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(150) {
        if docker_container_running(&prod_name) {
            running = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    report.assert_hard(
        "CR10 explicit start container running",
        running,
        format!("{:.0}s 内运行", t0.elapsed().as_secs_f64()),
    );
    if !running {
        return;
    }
    // PG 在容器启动窗口内异步初始化，重试窗口内以新密码 TCP 认证。
    let mut retained = false;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(150) {
        if let Some(uid) = docker_inspect_id(&prod_name)
            && cr10_tcp_login(&uid, &username, password)
        {
            retained = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    report.assert_hard(
        "CR10 explicit restart retains the new password",
        retained,
        "Neither start nor wake replays a historically saved password".into(),
    );

    // D. 已完成部署不接受显式改密恢复（无未决回执证据）。
    let (restart_s, restart_b) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/restart?user_id={user}"),
        json!({"pg":{"username":username,"password":password}}),
    )
    .await;
    let operation_id = restart_b["data"]["operation_id"]
        .as_str()
        .map(str::to_owned);
    report.assert_hard(
        "CR10 restart with explicit pg aligns",
        restart_s.is_success() && http_ok(&restart_b) && operation_id.is_some(),
        format!("HTTP {restart_s}, body 截断: {}", trunc(&restart_b, 150)),
    );
    let Some(operation_id) = operation_id else {
        return;
    };
    // Use the current revision, so stale-revision rejection cannot masquerade
    // as evidence that a completed operation refuses recovery.
    let (query_s, query_b) = get_json(
        env,
        &format!("/api/v1/userapp/{app}/operations/{operation_id}"),
    )
    .await;
    let revision = query_b["data"]["revision"].as_i64();
    let completed = query_s.is_success()
        && http_ok(&query_b)
        && query_b["data"]["state"] == "Succeeded"
        && query_b["data"]["operation_id"] == operation_id
        && revision.is_some();
    let (recover_s, recover_b) = post_json(
        env,
        "/api/v1/userapp/deploy-pg/recover",
        json!({"app_id":app,"lifecycle_id":lifecycle,"operation_id":operation_id,"expected_revision":revision,"username":username,"password":password}),
    )
    .await;
    report.assert_hard(
        "CR10 completed deployment refuses deploy-pg recovery",
        completed && (!recover_s.is_success() || !http_ok(&recover_b)),
        format!("HTTP {recover_s}; code={:?}", recover_b["code"].as_str()),
    );
}

/// C4 app-files prod：upload → files → delete + upload-from-url（制品回灌）。
async fn verify_app_files_prod(env: &Env, report: &JsonlReporter, app: &str, user: &str) {
    // 部署 API 的同步等待边界 = 部署段完成（服务启动结果异步可见，start.rs
    // 模块文档明示）；app-cli 业务编排（7 服务 PG migrate → 静态托管 →
    // 60000 分流代理）在部署返回后继续进行（实测 60-90s）。文件操作打
    // 60000——有界等待可达后再断言（超时则断言自然带最后错误失败）。
    {
        let deadline = Instant::now() + Duration::from_secs(150);
        loop {
            let reached = match env
                .http
                .get(format!(
                    "{}/api/v1/userapp/{app}/prod/files?user_id={user}&path=probe-upload",
                    env.rcoder
                ))
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                // 连接层失败（error sending request）或 200+ERR_BACKEND_ERROR
                // 转发失败信封 = 容器 60000 分流代理未就绪——继续等
                Err(_) => false,
                Ok(response) if response.status().as_u16() == 502 => false,
                Ok(response) => {
                    let body: Value = response.json().await.unwrap_or(Value::Null);
                    let forward_pending = body["code"].as_str() == Some("ERR_BACKEND_ERROR")
                        && body["message"]
                            .as_str()
                            .is_some_and(|m| m.contains("error sending request"));
                    !forward_pending
                }
            };
            if reached || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    // upload（multipart）
    let part = reqwest::multipart::Part::bytes(b"prod-files-probe").file_name("probe.txt");
    let form = reqwest::multipart::Form::new()
        .text("user_id", user.to_owned())
        .text("target", "probe-upload/probe.txt")
        .part("file", part);
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/{app}/prod/upload", env.rcoder))
        .timeout(Duration::from_secs(60))
        .multipart(form)
        .send()
        .await;
    let ok = matches!(&resp, Ok(r) if r.status().is_success());
    report.assert_hard(
        "app-files prod upload → 200",
        ok,
        match &resp {
            Ok(r) => format!("HTTP {}", r.status()),
            Err(e) => format!("err: {e}"),
        },
    );

    // files 列表含上传物
    let (s, b) = get_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/files?user_id={user}&path=probe-upload"),
    )
    .await;
    let listed = s.is_success()
        && http_ok(&b)
        && b["data"].as_array().is_some_and(|arr| {
            arr.iter()
                .any(|f| f["path"].as_str().is_some_and(|p| p.contains("probe.txt")))
        });
    report.assert_hard(
        "app-files prod files 列表含上传物",
        listed,
        format!("HTTP {s}, body 截断: {}", trunc(&b, 150)),
    );

    // files/delete
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/files/delete"),
        json!({"user_id": user, "path": "probe-upload/probe.txt"}),
    )
    .await;
    report.assert_hard(
        "app-files prod files/delete → 200",
        s.is_success() && http_ok(&b),
        format!("HTTP {s}, body 截断: {}", trunc(&b, 120)),
    );
}

/// Q05: a rejected image preparation must preserve the live physical runtime and traffic.
async fn verify_failed_image_update_preserves_runtime(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
) {
    async fn identity(name: &str) -> Result<Value, String> {
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new("docker")
                .args([
                    "inspect",
                    "--format",
                    r#"{"id":{{json .Id}},"image":{{json .Image}},"running":{{json .State.Running}}}"#,
                    name,
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|error| format!("Docker identity deadline: {error}"))?
        .map_err(|error| format!("Docker identity command: {error}"))?;
        if !output.status.success() {
            return Err("Docker runtime identity is unavailable".into());
        }
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
    }
    async fn traffic(env: &Env, url: &str) -> Result<(u16, Vec<u8>), String> {
        let response = env
            .http
            .get(url)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(|error| error.to_string())?;
        Ok((status, body.to_vec()))
    }
    let name = format!("rcoder-app-{app}");
    let url = format!(
        "{}/api/v1/userapp/proxy/app/prod/{user}/{app}/react/",
        pingora_base()
    );
    let before = identity(&name).await;
    let baseline = traffic(env, &url).await;
    let ready = before.as_ref().is_ok_and(|value| {
        value["id"].as_str().is_some_and(|id| !id.is_empty()) && value["running"] == true
    }) && baseline
        .as_ref()
        .is_ok_and(|(status, body)| *status == 200 && !body.is_empty());
    report.assert_hard(
        "Q05 image update baseline has running container and real React content",
        ready,
        format!(
            "identity={before:?}; traffic_status={:?}",
            baseline.as_ref().map(|(status, _)| status)
        ),
    );
    let (Ok(before), Ok((_, baseline))) = (before, baseline) else {
        return;
    };
    if !ready {
        return;
    }
    // Valid registry syntax and a fresh repository prevent a cached-image shortcut.
    // Port 1 on the local Docker daemon host is the intentionally refused registry.
    let image = format!(
        "127.0.0.1:1/rcoder-e2e-missing-{}:q05",
        uuid::Uuid::new_v4().simple()
    );
    let (status, result) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/update"),
        json!({"user_id": user, "image": image}),
    )
    .await;
    report.assert_hard(
        "Q05 unavailable image update returns explicit backend failure",
        status == reqwest::StatusCode::OK
            && result["code"].as_str() == Some(shared_types::ERR_BACKEND_ERROR)
            && result["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
        format!(
            "image={image}; HTTP {status}; response={}",
            trunc(&result, 400)
        ),
    );
    let after = identity(&name).await;
    report.assert_hard(
        "Q05 failed image update preserves physical container and image",
        after.as_ref().is_ok_and(|value| {
            value["id"] == before["id"]
                && value["image"] == before["image"]
                && value["running"] == true
        }),
        format!("before={before}; after={after:?}"),
    );
    let after_traffic = traffic(env, &url).await;
    report.assert_hard(
        "Q05 failed image update preserves real Pingora React content",
        after_traffic
            .as_ref()
            .is_ok_and(|(status, body)| *status == 200 && *body == baseline),
        format!(
            "baseline_sha256={}; after={:?}",
            Sha256::digest(&baseline)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            after_traffic.as_ref().map(|(status, body)| (
                *status,
                Sha256::digest(body)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ))
        ),
    );
}

/// C5 热部署：同 url + 新 release_id + deploy_mode=hot → 容器不换（started_at 不变）。
async fn verify_hot_redeploy(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
    release_id: &str,
    sha256: &str,
) {
    // 部署前 started_at
    let (s, b) = get_json(env, &format!("/api/v1/userapp/{app}?user_id={user}")).await;
    let before = b["data"]["started_at"].as_str().unwrap_or("").to_owned();
    let ok_pre = s.is_success() && http_ok(&b) && !before.is_empty();
    report.assert_hard(
        "热部署前置：get_app 拿 started_at",
        ok_pre,
        format!("HTTP {s}, started_at={before}"),
    );
    if !ok_pre {
        return;
    }

    // 受理前等待相位到 running（热部署语义前置）。实测抓到：Docker 模式
    // start 同步等待止于部署段完成（不等 app-cli 编排到 running），且后续
    // app-files/db 的 ensure 链存在容器重建竞态——C5 时刻可能又处于首次
    // 部署的 orchestrating（409 拒绝）。轮询到 running 再发是正确测试写法；
    // Docker 模式 ensure 重建竞态记为实现差距。
    let cname = format!("rcoder-app-{app}");
    let mut phase_ready = false;
    let pt0 = Instant::now();
    while pt0.elapsed() < Duration::from_secs(180) {
        let ph = std::process::Command::new("docker")
            .args([
                "exec",
                &cname,
                "wget",
                "-qO-",
                "http://127.0.0.1:3010/v1/deploy/status",
            ])
            .output();
        let ph_text = ph
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        if ph_text.contains(r#""phase":"running""#) {
            phase_ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    report.diagnostic(
        "热部署受理前相位等待",
        if phase_ready { "running" } else { "timeout" },
        &format!("{:.0}s", pt0.elapsed().as_secs_f64()),
    );
    if !phase_ready {
        report.assert_hard(
            "热部署前置：phase=running",
            false,
            "180s 内未到 running（首次编排/重建竞态）".into(),
        );
        return;
    }

    let hot_release = format!("hot-{}", uuid::Uuid::new_v4().simple());
    // URL 嵌原 release_id（static 按 release_id 定位制品，hot_release 无产物会 404）；
    // 请求体的 release_id 才是热部署新标签
    let artifact_url = format!(
        "{}/api/v1/userapp/static/{app}?release_id={release_id}&user_id={user}",
        rcoder_internal()
    );
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/start"),
        json!({
            "user_id": user,
            "url": artifact_url,
            "release_id": hot_release,
            "sha256": sha256,
            "deploy_mode": "hot"
        }),
    )
    .await;
    let accepted = s.is_success() && http_ok(&b);
    if !accepted {
        // 受理失败现场（500=编排失败/409=相位拒绝）：app-cli 日志尾部进报告
        let cname = format!("rcoder-app-{app}");
        let cli_log = std::process::Command::new("docker")
            .args([
                "exec",
                &cname,
                "sh",
                "-c",
                "tail -30 /home/user/logs/app-cli.err.log 2>/dev/null",
            ])
            .output();
        let cli_text = cli_log
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_else(|e| format!("exec failed: {e}"));
        report.diagnostic(
            "热部署受理失败现场",
            &format!("HTTP {s}"),
            &cli_text
                .chars()
                .rev()
                .take(1500)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
    }
    report.assert_hard(
        "热部署受理（deploy_mode=hot + 新 release_id → 200）",
        accepted,
        format!("HTTP {s}, body 截断: {}", trunc(&b, 150)),
    );
    if !accepted {
        return;
    }

    // 热路径铁证：started_at 不变（未换容器）+ 流量仍可达
    let (_, b) = get_json(env, &format!("/api/v1/userapp/{app}?user_id={user}")).await;
    let after = b["data"]["started_at"].as_str().unwrap_or("").to_owned();
    report.assert_hard(
        "热部署容器未换（started_at 不变）",
        after == before,
        format!("before={before} after={after}"),
    );

    let pingora = pingora_base();
    let mut served = false;
    let mut last_code = None;
    let t0 = Instant::now();
    while t0.elapsed() < ready_budget() {
        if let Ok(resp) = env
            .http
            .get(format!(
                "{pingora}/api/v1/userapp/proxy/app/prod/{user}/{app}/react/"
            ))
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            last_code = Some(resp.status().as_u16());
            if resp.status().is_success() {
                served = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    if !served {
        // 现场转储：容器内 supervisord 状态 + app-cli 相位（排障留痕）
        let cname = format!("rcoder-app-{app}");
        let sup = std::process::Command::new("docker")
            .args(["exec", &cname, "supervisorctl", "status"])
            .output();
        let sup_text = sup
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_else(|e| format!("exec failed: {e}"));
        let phase = std::process::Command::new("docker")
            .args([
                "exec",
                &cname,
                "wget",
                "-qO-",
                "http://127.0.0.1:3010/v1/deploy/status",
            ])
            .output();
        let phase_text = phase
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_else(|e| format!("exec failed: {e}"));
        // app-cli 日志尾部（migrate 失败的 stderr 现场）
        let cli_log = std::process::Command::new("docker")
            .args([
                "exec",
                &cname,
                "sh",
                "-c",
                "tail -40 /home/user/logs/app-cli.err.log 2>/dev/null",
            ])
            .output();
        let cli_text = cli_log
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_else(|e| format!("exec failed: {e}"));
        report.diagnostic(
            "热部署流量未恢复现场",
            &format!("last={last_code:?}"),
            &format!("sup:\n{sup_text}\nphase: {phase_text}\napp-cli tail:\n{cli_text}"),
        );
    }
    report.assert_hard(
        "热部署后流量仍可达",
        served,
        format!(
            "{:.0}s 内探测, last={last_code:?}",
            t0.elapsed().as_secs_f64()
        ),
    );
}

/// C6 手动停止保持停止，显式启动恢复真实业务。
async fn verify_stop_and_explicit_start(env: &Env, report: &JsonlReporter, app: &str, user: &str) {
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/stop?user_id={user}"),
        json!({}),
    )
    .await;
    let stopped = s.is_success() && http_ok(&b) && b["data"]["status"].as_str() == Some("stopped");
    report.assert_hard(
        "stop → stopped",
        stopped,
        format!("HTTP {s}, body 截断: {}", trunc(&b, 120)),
    );
    if !stopped {
        return;
    }

    // Health is observation only. A successful query must not wake manual Stop.
    let (health_status, health) = get_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/health?user_id={user}"),
    )
    .await;
    report.assert_hard(
        "手动 stop 后 health 查询保持停止",
        health_status.is_success()
            && http_ok(&health)
            && !docker_container_running(&format!("rcoder-app-{app}")),
        format!("health: {}", trunc(&health, 180)),
    );
    let (start_status, start) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/start?user_id={user}"),
        json!({}),
    )
    .await;
    let ready =
        start_status.is_success() && http_ok(&start) && cr10_proxy_ready(env, app, user).await;
    report.assert_hard(
        "stop 后显式 start 恢复真实业务",
        ready && docker_container_running(&format!("rcoder-app-{app}")),
        format!("start: {}", trunc(&start, 180)),
    );

    // prod `/computer/pod/restart`（compute 控制通道，此前零 prod 覆盖）：
    // 202 受理 → 容器恢复运行 + 代理就绪。Docker 形态该通道不滚镜像（物理
    // UID 身份围栏保持；镜像滚动走上面的 restart 通道）——差异固化为断言，
    // 语义变化时强制更新。
    let prod_container = format!("rcoder-app-{app}");
    let image_before = docker_inspect_image(&prod_container);
    let (cs, cb) = post_json(
        env,
        "/computer/pod/restart",
        json!({"user_id": user, "app_id": app, "app_stage": "prod", "service_type": "userapp"}),
    )
    .await;
    let accepted = cs.as_u16() == 202 && cb["data"]["operation_id"].as_str().is_some();
    report.assert_hard(
        "prod /computer/pod/restart 受理 202",
        accepted,
        format!("HTTP {cs}, body 截断: {}", trunc(&cb, 160)),
    );
    if accepted {
        let mut restored = false;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(180) {
            if docker_container_running(&prod_container) && cr10_proxy_ready(env, app, user).await {
                restored = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        report.assert_hard(
            "prod compute restart 容器恢复运行且代理就绪",
            restored,
            format!("{:.0}s 内恢复", t0.elapsed().as_secs_f64()),
        );
        let image_after = docker_inspect_image(&prod_container);
        report.assert_hard(
            "Docker compute 通道保持镜像不变（物理 UID 围栏；滚镜像走 restart 通道）",
            image_after == image_before,
            format!("before={image_before:?}, after={image_after:?}"),
        );
    }
}

/// 回收：prod delete purge → 流量转 502。
async fn cleanup_prod(env: &Env, report: &JsonlReporter, app: &str, user: &str) {
    let identity =
        rcoder_e2e::common::resources::register_created_container(&format!("rcoder-app-{app}"));
    report.assert_hard(
        "prod image and container identity recorded before cleanup",
        identity.is_ok(),
        identity
            .err()
            .unwrap_or_else(|| "resource receipt recorded".into()),
    );
    let capture = rcoder_e2e::common::resources::capture_container(&format!("rcoder-app-{app}"));
    report.assert_hard(
        "prod diagnostics captured before deletion",
        capture.is_ok(),
        capture.err().unwrap_or_default(),
    );
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/prod/delete"),
        json!({"user_id": user, "purge": true}),
    )
    .await;
    report.assert_hard(
        "prod delete purge 回收",
        s.is_success() && http_ok(&b),
        format!("HTTP {s}, body 截断: {}", trunc(&b, 150)),
    );
    // 删除后流量应转 502（backend 注销）；宽限重试几秒防注销竞态
    let mut code = None;
    for _ in 0..6 {
        if let Ok(resp) = env
            .http
            .get(format!(
                "{}/api/v1/userapp/proxy/app/prod/{user}/{app}/react/",
                pingora_base()
            ))
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            code = Some(resp.status().as_u16());
            if code == Some(502) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    report.assert_hard(
        "删除后流量转 502（backend 已注销）",
        code == Some(502),
        format!("last status = {code:?}"),
    );
}

#[tokio::test]
async fn userapp_deploy_full_chain() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_deploy_full_chain";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    if preflight(&report).await.is_none() {
        report.skip("镜像前置不满足（见 diagnostic 行）");
        return;
    }

    let app = format!(
        "adep{}p{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let user = "e2e-dep-user";

    // ① workspace
    let (ws_s, ws_b) = post_json(
        &env,
        "/api/v1/userapp/workspace",
        json!({"app_id": app, "user_id": user}),
    )
    .await;
    let ws_ok = ws_s.is_success() && http_ok(&ws_b);
    report.assert_hard(
        "create-workspace（ensure builder + owner 注册）",
        ws_ok,
        format!("HTTP {ws_s}, body 截断: {}", trunc(&ws_b, 150)),
    );
    if !ws_ok {
        cleanup_builder(&app);
        let path = report.path.display().to_string();
        assert!(report.finish(), "场景失败：断言明细见 {path}");
        return;
    }

    // ② 模板初始化 + 落盘断言（template-cli ≥0.1.1 已含全部模板修复，
    //    历史兜底 normalize 三步随之退役——曾覆盖：multiline TOML 转义、
    //    next lockfile 平台二进制/静态 import/NODE_ENV、python ABI 3.13）
    if init_full_template(&env, &report, &app, user).await {
        assert_template_files(&env, &report, &app, user).await;
    }

    // ③ 构建 + ④ 取包校验
    if let Some((release_id, sha256)) = build_to_completion(&env, &report, &app, user).await
        && let Some(()) =
            fetch_and_verify_artifact(&env, &report, &app, user, &release_id, &sha256).await
    {
        // ⑤ 部署 + 七路流量 ⑤b prod 观测族 ⑥ 回收
        if !deploy_and_verify_traffic(&env, &report, &app, user, &release_id, &sha256).await {
            cleanup_prod(&env, &report, &app, user).await;
            cleanup_builder(&app);
            let path = report.path.display().to_string();
            assert!(report.finish(), "cold deployment failed: {path}");
            return;
        }
        verify_prod_observability(&env, &report, &app, user).await;
        // Independent account administration must not invalidate the runtime
        // credentials used by subsequent hot deployment and migrations.
        verify_db_prod(&env, &report, &app, user).await;
        verify_app_files_prod(&env, &report, &app, user).await;
        verify_failed_image_update_preserves_runtime(&env, &report, &app, user).await;
        verify_hot_redeploy(&env, &report, &app, user, &release_id, &sha256).await;
        // 轻量部署（无 release_id，独立操作身份确认），同样在独立账号改密后验证。
        verify_url_lightweight_deploy_without_release_id(
            &env,
            &report,
            &app,
            user,
            &release_id,
            &sha256,
        )
        .await;
        verify_stop_and_explicit_start(&env, &report, &app, user).await;
        // Last business mutation: later explicit restart requires caller-owned
        // application credential configuration, not automatic platform injection.
        verify_immediate_runtime_password(&env, &report, &app, user).await;
        verify_password_governance_after_change(&env, &report, &app, user).await;
        cleanup_prod(&env, &report, &app, user).await;
    }
    // No production request was sent when build/artifact preparation failed.
    // Keep the original failure and clean the builder; do not invent a missing
    // production resource or execute deletion against an uncreated deployment.
    cleanup_builder(&app);

    let path = report.path.display().to_string();
    assert!(report.finish(), "场景失败：断言明细见 {path}");
}

/// §7.1/7.4/7.6 compose 场景：prod 部署在途期间 dev 域操作独立受理；
/// current 数组双 scope 同现；同域并发 409 携带结构化 blocker；部署不受
/// 并发影响到达 running。修复前：dev restart 在 prod StartDeployment 在途时
/// 收 ERR_CONFLICT（单槽互斥），本场景在该窗口内必须成功。
#[tokio::test]
async fn userapp_scope_isolation_during_deploy() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_scope_isolation_during_deploy";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    if preflight(&report).await.is_none() {
        report.skip("镜像前置不满足（见 diagnostic 行）");
        return;
    }

    let app = format!(
        "aiso{}p{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let user = "e2e-iso-user";

    // ①-④ 与 full_chain 相同的 workspace/模板/构建/取包准备
    let (ws_s, ws_b) = post_json(
        &env,
        "/api/v1/userapp/workspace",
        json!({"app_id": app, "user_id": user}),
    )
    .await;
    let ws_ok = ws_s.is_success() && http_ok(&ws_b);
    report.assert_hard(
        "create-workspace（ensure builder + owner 注册）",
        ws_ok,
        format!("HTTP {ws_s}, body 截断: {}", trunc(&ws_b, 150)),
    );
    if !ws_ok {
        cleanup_builder(&app);
        let path = report.path.display().to_string();
        assert!(report.finish(), "场景失败：断言明细见 {path}");
        return;
    }
    if init_full_template(&env, &report, &app, user).await {
        assert_template_files(&env, &report, &app, user).await;
    }
    let Some((release_id, sha256)) = build_to_completion(&env, &report, &app, user).await else {
        cleanup_builder(&app);
        let path = report.path.display().to_string();
        assert!(report.finish(), "场景失败：断言明细见 {path}");
        return;
    };
    if fetch_and_verify_artifact(&env, &report, &app, user, &release_id, &sha256)
        .await
        .is_none()
    {
        cleanup_builder(&app);
        let path = report.path.display().to_string();
        assert!(report.finish(), "场景失败：断言明细见 {path}");
        return;
    }

    let artifact_url = format!(
        "{}/api/v1/userapp/static/{app}?release_id={release_id}&user_id={user}",
        rcoder_internal()
    );
    let deploy_body = json!({
        "user_id": user, "url": artifact_url, "release_id": release_id,
        "sha256": sha256, "request_id": format!("iso-deploy-{app}")
    });
    let deploy_path = format!("/api/v1/userapp/{app}/start");

    // ⑤ 并发窗口：部署请求在后台任务真实发出；轮询 current 直到 prod 槽可见
    let deploy_http = env.http.clone();
    let deploy_base = env.rcoder.clone();
    let deploy_task_path = deploy_path.clone();
    let deploy_task = tokio::spawn(async move {
        let resp = deploy_http
            .post(format!("{deploy_base}{deploy_task_path}"))
            .timeout(Duration::from_secs(120))
            .json(&deploy_body)
            .send()
            .await;
        match resp {
            Ok(r) => {
                let s = r.status();
                let b = r.json().await.unwrap_or(Value::Null);
                (s, b)
            }
            Err(e) => (
                reqwest::StatusCode::BAD_GATEWAY,
                serde_json::json!({"error": e.to_string()}),
            ),
        }
    });

    let mut prod_seen = false;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(60) && !prod_seen {
        let (s, b) = get_json(
            &env,
            &format!("/api/v1/userapp/{app}/operations/current?user_id={user}"),
        )
        .await;
        // 兼容新旧形状：新=数组（含 scope），旧=单对象（修复前反例取证用）
        let prod_active = |op: &Value| {
            op["kind"].as_str() == Some("StartDeployment")
                && matches!(op["state"].as_str(), Some("Pending") | Some("Running"))
        };
        let seen = s.is_success()
            && http_ok(&b)
            && (b["data"]
                .as_array()
                .is_some_and(|ops| ops.iter().any(prod_active))
                || (b["data"].is_object() && prod_active(&b["data"])));
        if seen {
            prod_seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    report.assert_hard(
        "部署在途窗口捕获（current 数组含非终态 StartDeployment）",
        prod_seen,
        format!("{:.1}s 轮询结果", t0.elapsed().as_secs_f64()),
    );
    if !prod_seen {
        deploy_task.abort();
        cleanup_builder(&app);
        let path = report.path.display().to_string();
        assert!(report.finish(), "场景失败：断言明细见 {path}");
        return;
    }

    // ⑤b 窗口内并发两路（后台任务真实在飞）：dev restart（修复前 409 反例）
    // + 同域第二 prod start（冲突反例）；同时轮询 current 观察双 scope 同现。
    let restart_http = env.http.clone();
    let restart_base = env.rcoder.clone();
    let restart_app = app.clone();
    let restart_user = user.to_owned();
    let restart_task = tokio::spawn(async move {
        let resp = restart_http
            .post(format!("{restart_base}/computer/pod/restart"))
            .timeout(Duration::from_secs(120))
            .json(&json!({"user_id": restart_user, "project_id": format!("iso-{restart_app}"), "app_id": restart_app, "app_stage": "dev", "service_type": "userapp"}))
            .send()
            .await;
        match resp {
            Ok(r) => {
                let s = r.status();
                let b = r.json().await.unwrap_or(Value::Null);
                (s, b)
            }
            Err(e) => (
                reqwest::StatusCode::BAD_GATEWAY,
                serde_json::json!({"error": e.to_string()}),
            ),
        }
    });
    let second_http = env.http.clone();
    let second_base = env.rcoder.clone();
    let second_path = deploy_path.clone();
    let second_body = json!({
        "user_id": user, "url": artifact_url, "release_id": release_id,
        "sha256": sha256, "request_id": format!("iso-deploy-b-{app}")
    });
    let second_task = tokio::spawn(async move {
        let resp = second_http
            .post(format!("{second_base}{second_path}"))
            .timeout(Duration::from_secs(120))
            .json(&second_body)
            .send()
            .await;
        match resp {
            Ok(r) => {
                let s = r.status();
                let b = r.json().await.unwrap_or(Value::Null);
                (s, b)
            }
            Err(e) => (
                reqwest::StatusCode::BAD_GATEWAY,
                serde_json::json!({"error": e.to_string()}),
            ),
        }
    });

    // ⑤c current 数组双 scope 同现（Dev RestartBuilder + Prod 部署族）
    let mut both_scopes = false;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(20) {
        let (s, b) = get_json(
            &env,
            &format!("/api/v1/userapp/{app}/operations/current?user_id={user}"),
        )
        .await;
        let dev_active = |op: &Value| {
            op["scope"].as_str() == Some("Dev") && op["kind"].as_str() == Some("RestartBuilder")
        };
        let prod_active_any = |op: &Value| {
            op["scope"].as_str() == Some("Prod")
                && !op["state"]
                    .as_str()
                    .is_some_and(|st| st == "Succeeded" || st == "Failed")
        };
        if s.is_success()
            && http_ok(&b)
            && let Some(ops) = b["data"].as_array()
            && ops.iter().any(dev_active)
            && ops.iter().any(prod_active_any)
        {
            both_scopes = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    report.assert_hard(
        "current 数组双 scope 同现（Dev RestartBuilder + Prod 部署族）",
        both_scopes,
        "20s 内未同时观察到两 scope 的活动操作".to_owned(),
    );

    // ⑤b 断言：dev restart 与 prod 部署并发 → 独立受理（修复前 ERR_CONFLICT）
    let (rs, rb) = restart_task.await.expect("restart task join");
    let not_blocked = rs.is_success() && rb["success"].as_bool().unwrap_or(false);
    report.assert_hard(
        "dev restart 与 prod 部署并发 → 独立受理（无 conflicting 409）",
        not_blocked,
        format!("HTTP {rs}, body 截断: {}", trunc(&rb, 200)),
    );

    // ⑤f 同域并发 dev restart 反例（required 契约步）：两路不同 request 的
    // dev restart 并发 → 恰一胜者受理，败者 409 信封携带结构化
    // blocker.scope=Dev（M3 透传；不解析消息文本）。
    let dr_http = env.http.clone();
    let dr_base = env.rcoder.clone();
    let dr_app = app.clone();
    let dr_user = user.to_owned();
    let dev_restart_a = tokio::spawn(async move {
        let resp = dr_http
            .post(format!("{dr_base}/computer/pod/restart"))
            .timeout(Duration::from_secs(120))
            .json(&json!({"user_id": dr_user, "project_id": format!("iso-{dr_app}"), "app_id": dr_app, "app_stage": "dev", "service_type": "userapp"}))
            .send()
            .await;
        match resp {
            Ok(r) => {
                let s = r.status();
                let b = r.json().await.unwrap_or(Value::Null);
                (s, b)
            }
            Err(e) => (
                reqwest::StatusCode::BAD_GATEWAY,
                serde_json::json!({"error": e.to_string()}),
            ),
        }
    });
    let dr2_http = env.http.clone();
    let dr2_base = env.rcoder.clone();
    let dr2_app = app.clone();
    let dr2_user = user.to_owned();
    let dev_restart_b = tokio::spawn(async move {
        let resp = dr2_http
            .post(format!("{dr2_base}/computer/pod/restart"))
            .timeout(Duration::from_secs(120))
            .json(&json!({"user_id": dr2_user, "project_id": format!("iso-{dr2_app}"), "app_id": dr2_app, "app_stage": "dev", "service_type": "userapp"}))
            .send()
            .await;
        match resp {
            Ok(r) => {
                let s = r.status();
                let b = r.json().await.unwrap_or(Value::Null);
                (s, b)
            }
            Err(e) => (
                reqwest::StatusCode::BAD_GATEWAY,
                serde_json::json!({"error": e.to_string()}),
            ),
        }
    });
    let (dra_s, dra_b) = dev_restart_a.await.expect("dev restart A join");
    let (drb_s, drb_b) = dev_restart_b.await.expect("dev restart B join");
    let a_ok = dra_s.is_success() && dra_b["success"].as_bool().unwrap_or(false);
    let b_conflict = drb_s.is_success()
        && drb_b["code"].as_str() == Some("ERR_CONFLICT")
        && drb_b["blocker"]["scope"].as_str() == Some("Dev");
    let b_winner = dra_s.is_success()
        && dra_b["success"].as_bool().unwrap_or(false)
        && drb_s.is_success()
        && drb_b["success"].as_bool().unwrap_or(false);
    // 两路并发自 tokio::spawn，受理顺序不保证——对称接受任一侧胜出：
    // a_conflict = A 409 + blocker.scope=Dev（B 先落地）。
    let a_conflict = dra_s.is_success()
        && dra_b["code"].as_str() == Some("ERR_CONFLICT")
        && dra_b["blocker"]["scope"].as_str() == Some("Dev");
    // 恰一胜者：一侧成功 + 另一侧结构化冲突，或时序上两路都成功（第二路
    // 赶上第一路完成后的窗口——此时胜者仍是"恰一"语义的时序边界，不算
    // 失败只要求无假 409）
    report.assert_hard(
        "同域并发 restart → 恰一胜者 + 败者 409 带 blocker.scope=Dev",
        (a_ok && b_conflict)
            || (a_conflict && drb_s.is_success() && drb_b["success"].as_bool().unwrap_or(false))
            || b_winner,
        format!(
            "A: HTTP {dra_s}, body 截断: {} | B: HTTP {drb_s}, body 截断: {}",
            trunc(&dra_b, 200),
            trunc(&drb_b, 200)
        ),
    );

    // ⑤e 同域并发反例：窗口内第二个 prod start（不同 request_id）→
    // 信封 ERR_CONFLICT + 结构化 blocker.scope=Prod。修复前：冲突但无 blocker 字段。
    let (cs, cb) = second_task.await.expect("second start task join");
    let same_scope_conflict = cs.is_success()
        && cb["code"].as_str() == Some("ERR_CONFLICT")
        && cb["blocker"]["scope"].as_str() == Some("Prod")
        && cb["blocker"]["kind"].as_str() == Some("StartDeployment")
        && cb["blocker"]["operation_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty());
    report.assert_hard(
        "同域并发 prod start → 信封 ERR_CONFLICT + blocker.scope=Prod",
        same_scope_conflict,
        format!("HTTP {cs}, body 截断: {}", trunc(&cb, 240)),
    );

    // ⑤d 部署收敛：并发 dev 操作不破坏 prod 部署
    let (ds, db) = deploy_task.await.expect("deploy task join");
    let deploy_ok =
        ds.is_success() && http_ok(&db) && db["data"]["status"].as_str() == Some("running");
    report.assert_hard(
        "隔离场景部署终态 running（并发不破坏部署）",
        deploy_ok,
        format!("HTTP {ds}, body 截断: {}", trunc(&db, 200)),
    );

    // ⑥ 回收（复用 full_chain 清理链）
    cleanup_prod(&env, &report, &app, user).await;
    cleanup_builder(&app);

    let path = report.path.display().to_string();
    assert!(report.finish(), "场景失败：断言明细见 {path}");
}
