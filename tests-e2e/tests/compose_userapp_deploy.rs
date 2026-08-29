//! compose 环境 UserApp **部署主流程全链**：template-cli 全量模板初始化 →
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
        .await
        .expect("http post");
    let status = resp.status();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
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

fn trunc(v: &Value, n: usize) -> String {
    let s = v.to_string();
    s.chars().take(n).collect()
}

/// 显式清理 builder 容器（docker rm；compose 冒烟防残留）。
fn cleanup_builder(app_id: &str) {
    let name = format!("rcoder-app-builder-{app_id}");
    std::process::Command::new("docker")
        .args(["rm", "-f", &name])
        .output()
        .ok();
}

/// 前置探测：app-runtime 镜像存在 + agent-runner 镜像含 template-cli。
/// 返回 None = 前置不满足（调用方 skip）。
async fn preflight(report: &JsonlReporter) -> Option<()> {
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
        // 失败留痕：失败服务（快照 current_service）的分服务日志尾部进报告
        //（tasks logs 端点 ?service= 定位；快照 error 本身已嵌输出尾部——
        // 此处再拉完整日志行号分页视角，排查线索，不额外判死）
        let svc = data["current_service"].as_str().unwrap_or_default();
        let (_, logs) = get_json(
            env,
            &format!(
                "/api/v1/userapp/tasks/{task_id}/logs?app_id={app}&user_id={user}&service={svc}&start_index=1"
            ),
        )
        .await;
        report.diagnostic(
            &format!("build 失败服务 [{svc}] 日志尾部"),
            &trunc(&logs, 3000),
            "tasks logs?service=current_service",
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
) -> Option<String> {
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
    ok.then_some(url)
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
    artifact_path: &str,
) {
    // 制品 URL 用容器可达形态（app 容器并入 compose 主网络，按服务名回拉 rcoder）
    let artifact_url = format!("{}{artifact_path}", rcoder_internal());
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app}/start"),
        json!({"user_id": user, "url": artifact_url, "release_id": release_id, "sha256": sha256}),
    )
    .await;
    let started = s.is_success() && http_ok(&b);
    report.assert_hard(
        "start(url) 部署受理（200 + Running）",
        started && b["data"]["status"].as_str() == Some("running"),
        format!("HTTP {s}, body 截断: {}", trunc(&b, 200)),
    );
    if !started {
        return;
    }

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
    let base = format!("{pingora}/proxy/userapp/prod/{user}/{app}");
    let t0 = Instant::now();
    let mut pending: Vec<(&str, &str)> = probes.to_vec();
    let mut first_seen: Vec<(String, u128)> = Vec::new();
    while !pending.is_empty() && t0.elapsed() < ready_budget() {
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
}

/// 回收：prod delete purge → 流量转 502。
async fn cleanup_prod(env: &Env, report: &JsonlReporter, app: &str, user: &str) {
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
                "{}/proxy/userapp/prod/{user}/{app}/react/",
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
        "app-e2e-dep-{}p{}",
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
        && let Some(artifact_path) =
            fetch_and_verify_artifact(&env, &report, &app, user, &release_id, &sha256).await
    {
        // ⑤ 部署 + 七路流量 ⑥ 回收
        deploy_and_verify_traffic(
            &env,
            &report,
            &app,
            user,
            &release_id,
            &sha256,
            &artifact_path,
        )
        .await;
        cleanup_prod(&env, &report, &app, user).await;
    } else {
        cleanup_prod(&env, &report, &app, user).await;
    }
    cleanup_builder(&app);

    let path = report.path.display().to_string();
    assert!(report.finish(), "场景失败：断言明细见 {path}");
}
