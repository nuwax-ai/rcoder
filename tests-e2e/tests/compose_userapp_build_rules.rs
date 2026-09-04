//! compose 环境 Userapp **编译期规则**场景：static 产物路由一致性检查（错配
//! 构建失败 / 对齐形态通过）+ devbuild 三分派（devrun 自足跳过编译、未配
//! devrun 回落全量编译）。
//!
//! 运行: `cargo test -p rcoder-e2e --test compose_userapp_build_rules -- --test-threads=1`
//! （与 compose_sse 同门控：RCODER_URL /health 可达；无 LLM/npm 依赖——产物用
//! zip 直投 dist fixture + `sh -c` 原生命令，dev 服务用 python3 http.server）
//!
//! 覆盖点（对应 frontend-detector proxy_consistency + dev_mode::devbuild_argv）：
//! - static+proxy 服务构建后校验烧进 dist/index.html 的资源引用 vs
//!   [proxy].path/strip_prefix：base 逃逸（引用无前缀）→ 任务 failed 且 error
//!   含服务名/逃逸指引；对齐形态（strip=true+无前缀布局）→ completed（无误报）
//! - 源码态 dev/start：只配 [devrun] 的服务跳过编译（build marker 不落盘）、
//!   未配 [devrun] 的服务回落 [build].command（marker 落盘），编排整体 completed

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// 套件级串行锁：单节点资源天花板下多场景并行建 builder 容器会互相拖慢
/// （与 compose_userapp_dev 同款）。
static SCENARIO_GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn scenario_gate() -> tokio::sync::MutexGuard<'static, ()> {
    SCENARIO_GATE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

use rcoder_e2e::common::Env;
use rcoder_e2e::common::report::JsonlReporter;
use rcoder_e2e::common::scenario::assert_hard_all;
use serde_json::{Value, json};

fn http_ok(body: &Value) -> bool {
    body["code"].as_str() == Some("0000")
}

fn trunc(v: &Value, n: usize) -> String {
    let s = v.to_string();
    if s.len() <= n {
        s
    } else {
        format!("{}…", &s[..n])
    }
}

/// 场景内唯一 app_id（run_tag+pid 防跨进程撞名；与 compose_userapp_dev 同款，
/// 前缀 app- 对齐 logs 族命名约束）。
fn scoped_app(env: &Env, tag: &str) -> String {
    let short_tag: String = tag
        .split('-')
        .filter_map(|part| part.chars().next())
        .collect();
    format!(
        "app-e2e-br-{}-p{}-{}",
        &env.run_tag.replace('_', "")[..6],
        std::process::id() % 1000,
        short_tag
    )
    .chars()
    .take(37)
    .collect()
}

fn cleanup_builder(app_id: &str) {
    let name = format!("rcoder-app-builder-{app_id}");
    std::process::Command::new("docker")
        .args(["rm", "-f", &name])
        .output()
        .ok();
}

/// create-workspace（幂等起手；冷启动重试窗口与 compose_userapp_dev 同款）。
async fn create_workspace(env: &Env, report: &JsonlReporter, app_id: &str, user: &str) -> bool {
    let mut status = reqwest::StatusCode::INTERNAL_SERVER_ERROR;
    let mut body = Value::Null;
    for attempt in 0..3 {
        let resp = env
            .http
            .post(format!("{}/api/v1/userapp/workspace", env.rcoder))
            .timeout(Duration::from_secs(600))
            .json(&json!({"app_id": app_id, "user_id": user}))
            .send()
            .await;
        let Ok(resp) = resp else { continue };
        status = resp.status();
        body = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() && http_ok(&body) {
            break;
        }
        report.diagnostic(
            "ensure 冷启动重试（file-server 启动窗口）",
            &format!("attempt {attempt}"),
            &trunc(&body, 100),
        );
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
    let ok = status.is_success()
        && http_ok(&body)
        && body["data"]["container_name"]
            .as_str()
            .is_some_and(|n| n.contains(app_id));
    report.assert_hard(
        "create-workspace（ensure 开发容器）",
        ok,
        format!("HTTP {status}, body 截断: {}", trunc(&body, 120)),
    );
    ok
}

/// 以 zip 模板初始化 workspace（init-project-template，条目落 workspace 根）。
async fn init_zip_workspace(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
    entries: &[(&str, &str)],
) -> bool {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
    for (name, content) in entries {
        zw.start_file(*name, opts).unwrap();
        std::io::Write::write_all(&mut zw, content.as_bytes()).unwrap();
    }
    let zip_bytes = zw.finish().unwrap().into_inner();
    let part = reqwest::multipart::Part::bytes(zip_bytes).file_name("template.zip");
    let form = reqwest::multipart::Form::new()
        .text("app_id", app.to_owned())
        .text("user_id", user.to_owned())
        .text("enable_git", "false")
        .part("file", part);
    let resp = env
        .http
        .post(format!(
            "{}/api/v1/userapp/init-project-template",
            env.rcoder
        ))
        .timeout(Duration::from_secs(60))
        .header("X-App-Id", app)
        .multipart(form)
        .send()
        .await
        .expect("init zip");
    let ok = resp.status().is_success();
    report.assert_hard(
        "init 模板 zip（manifests + dist fixture 直投源码目录）",
        ok,
        format!("HTTP {}", resp.status()),
    );
    ok
}

/// 触发构建并轮询到终态，返回任务快照 data（未达终态 → None + hard 断言红）。
async fn build_to_terminal(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
    budget: Duration,
) -> Option<Value> {
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
    let accepted = status.is_success() && http_ok(&body) && !task_id.is_empty();
    report.assert_hard(
        "build 受理（200 + task_id）",
        accepted,
        format!("HTTP {status}, body 截断: {}", trunc(&body, 120)),
    );
    if !accepted {
        return None;
    }
    let t0 = Instant::now();
    while t0.elapsed() < budget {
        let resp = env
            .http
            .get(format!(
                "{}/api/v1/userapp/tasks/{task_id}?app_id={app}&user_id={user}",
                env.rcoder
            ))
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        if let Ok(r) = resp
            && r.status().is_success()
            && let Ok(b) = r.json::<Value>().await
            && let Some(st) = b["data"]["status"].as_str()
            && matches!(st, "failed" | "cancelled" | "completed")
        {
            report.diagnostic(
                "build 到达终态",
                &format!("{:.0}s, status={st}", t0.elapsed().as_secs_f64()),
                b["data"]["error"].as_str().unwrap_or(""),
            );
            return Some(b["data"].clone());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    report.assert_hard(
        "build 到达终态（预算内不挂死）",
        false,
        format!("{:.0}s 未到终态", t0.elapsed().as_secs_f64()),
    );
    None
}

/// resolve-file 探测 workspace 内相对路径是否存在（`{success, exists}` 裸信封；
/// 请求失败/信封异常 → None，由调用方断言红）。
async fn file_exists(env: &Env, app: &str, user: &str, file_path: &str) -> Option<bool> {
    let resp = env
        .http
        .get(format!(
            "{}/api/v1/userapp/resolve-file?app_id={app}&user_id={user}&file_path={file_path}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(15))
        .header("X-App-Id", app)
        .send()
        .await
        .expect("resolve-file");
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if body["success"].as_bool() != Some(true) {
        return None;
    }
    body["exists"].as_bool()
}

// ============================================================
// 场景 1：static 产物 base 逃逸 → build failed（错误自明服务与修复指引）
// ============================================================
#[tokio::test]
async fn userapp_static_proxy_escape_build_fails() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_static_proxy_escape";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "esc");
    let user = "e2e-br-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // dist fixture：引用 /assets/app.js 无 /web 前缀（= 构建工具 base 写成 /），
    // 但产物布局本身完整（assets/app.js 在）——唯一的错就是前缀逃逸
    let ws_manifest = "schema_version = 1\n\n[workspace]\nname = \"e2e-br-esc\"\n";
    let web_manifest = "schema_version = 1\n\n[project]\nservice_id = \"web\"\nname = \"Web Static\"\ntype = \"static\"\n\n[build]\ncommand = [\"sh\", \"-c\", \"test -f dist/index.html\"]\nartifact = \"dist\"\n\n[proxy]\npath = \"/web\"\nstrip_prefix = true\n";
    let index_html = "<!doctype html><html><head><script src=\"/assets/app.js\"></script></head><body></body></html>";
    if !init_zip_workspace(
        &env,
        &report,
        &app,
        user,
        &[
            ("workspace.manifest.toml", ws_manifest),
            ("web/project.manifest.toml", web_manifest),
            ("web/dist/index.html", index_html),
            ("web/dist/assets/app.js", "// stub"),
        ],
    )
    .await
    {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    let data = build_to_terminal(&env, &report, &app, user, Duration::from_secs(240)).await;
    let failed = data
        .as_ref()
        .and_then(|d| d["status"].as_str())
        .is_some_and(|st| st == "failed");
    report.assert_hard(
        "base 逃逸 → build 终态 = failed",
        failed,
        format!(
            "data 截断: {}",
            trunc(&data.clone().unwrap_or(Value::Null), 150)
        ),
    );
    let error = data
        .as_ref()
        .and_then(|d| d["error"].as_str())
        .unwrap_or("")
        .to_owned();
    let msg_ok = error.contains("web") && error.contains("逃逸") && error.contains("/web");
    report.assert_hard(
        "错误文案自明（服务名 + 逃逸 + 期望前缀）",
        msg_ok,
        format!("error: {error}"),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// 场景 2：对齐形态（strip=true + 无前缀布局）→ build completed（无误报回归）
// ============================================================
#[tokio::test]
async fn userapp_static_proxy_aligned_build_passes() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_static_proxy_aligned";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "ali");
    let user = "e2e-br-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 与场景 1 同布局，仅 index.html 引用带 /web 前缀（vite base=/web/ 的正确产物）
    let ws_manifest = "schema_version = 1\n\n[workspace]\nname = \"e2e-br-ali\"\n";
    let web_manifest = "schema_version = 1\n\n[project]\nservice_id = \"web\"\nname = \"Web Static\"\ntype = \"static\"\n\n[build]\ncommand = [\"sh\", \"-c\", \"test -f dist/index.html\"]\nartifact = \"dist\"\n\n[proxy]\npath = \"/web\"\nstrip_prefix = true\n";
    let index_html = "<!doctype html><html><head><script src=\"/web/assets/app.js\"></script><link rel=\"stylesheet\" href=\"/web/assets/app.css\"></head><body></body></html>";
    if !init_zip_workspace(
        &env,
        &report,
        &app,
        user,
        &[
            ("workspace.manifest.toml", ws_manifest),
            ("web/project.manifest.toml", web_manifest),
            ("web/dist/index.html", index_html),
            ("web/dist/assets/app.js", "// stub"),
            ("web/dist/assets/app.css", "/* stub */"),
        ],
    )
    .await
    {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    let data = build_to_terminal(&env, &report, &app, user, Duration::from_secs(240)).await;
    let completed = data
        .as_ref()
        .and_then(|d| d["status"].as_str())
        .is_some_and(|st| st == "completed");
    report.assert_hard(
        "对齐形态 → build 终态 = completed（一致性检查无误报）",
        completed,
        format!(
            "data 截断: {}",
            trunc(&data.clone().unwrap_or(Value::Null), 150)
        ),
    );
    let release_ok = data
        .as_ref()
        .is_some_and(|d| d["release_id"].as_str().is_some_and(|r| !r.is_empty()));
    report.assert_hard(
        "completed 快照含 release_id",
        release_ok,
        format!(
            "data 截断: {}",
            trunc(&data.clone().unwrap_or(Value::Null), 150)
        ),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// 场景 3：devbuild 三分派（源码态 dev/start）——devrun 自足跳过编译、
//          未配 devrun 回落 build.command，编排整体 completed
// ============================================================
#[tokio::test]
async fn userapp_devbuild_skip_and_fallback_source_mode() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_devbuild_rules";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "dbs");
    let user = "e2e-br-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 双服务：go-svc 未配 devrun（编译回落 [build].command → 落 marker）；
    // dev-svc 只配 devrun（编译跳过 → marker 不落盘）。build.command 的 touch
    // marker 即"编译发生过"的黑盒探针（源码态编译不校验 artifact，marker 是
    // 唯一可见证据）。运行侧均 python3 http.server + touch ready（B5 同款范式）。
    let ws_manifest = "schema_version = 1\n\n[workspace]\nname = \"e2e-br-dbs\"\n";
    let go_manifest = "schema_version = 1\n\n[project]\nservice_id = \"go-svc\"\nname = \"Go API\"\ntype = \"go\"\n\n[build]\ncommand = [\"sh\", \"-c\", \"touch built-go.marker\"]\nartifact = \"artifact.zip\"\n\n[run]\ncommand = [\"sh\", \"-c\", \"touch ready-go && exec python3 -m http.server $PORT --bind 0.0.0.0\"]\n\n[health]\nreadiness_path = \"/ready-go\"\n\n[proxy]\npath = \"/api/go/\"\nstrip_prefix = true\n";
    let dev_manifest = "schema_version = 1\n\n[project]\nservice_id = \"dev-svc\"\nname = \"Hot Reload\"\ntype = \"node\"\n\n[build]\ncommand = [\"sh\", \"-c\", \"touch built-dev.marker\"]\nartifact = \"artifact.zip\"\n\n[run]\ncommand = [\"sh\", \"-c\", \"touch ready-dev && exec python3 -m http.server $PORT --bind 0.0.0.0\"]\n\n[health]\nreadiness_path = \"/ready-dev\"\n\n[devrun]\ncommand = [\"sh\", \"-c\", \"touch ready-dev && exec python3 -m http.server $PORT --bind 0.0.0.0\"]\n\n[proxy]\npath = \"/dev/\"\n";
    if !init_zip_workspace(
        &env,
        &report,
        &app,
        user,
        &[
            ("workspace.manifest.toml", ws_manifest),
            ("go-svc/project.manifest.toml", go_manifest),
            ("dev-svc/project.manifest.toml", dev_manifest),
        ],
    )
    .await
    {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // dev/start（异步任务）
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/dev/start", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user}))
        .send()
        .await
        .expect("dev start");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let task_id = body["data"]["task_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    report.assert_hard(
        "dev/start 受理（task_id）",
        status.is_success() && http_ok(&body) && !task_id.is_empty(),
        format!("HTTP {status}, body 截断: {}", trunc(&body, 150)),
    );
    if task_id.is_empty() {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 轮询任务到终态（免编译应快；app-cli 冷启动留余量）
    let mut terminal: Option<(String, String)> = None;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(240) {
        let resp = env
            .http
            .get(format!(
                "{}/api/v1/userapp/tasks/{task_id}?app_id={app}&user_id={user}",
                env.rcoder
            ))
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        if let Ok(r) = resp
            && r.status().is_success()
            && let Ok(b) = r.json::<Value>().await
            && let Some(st) = b["data"]["status"].as_str()
            && matches!(st, "completed" | "failed" | "cancelled")
        {
            terminal = Some((
                st.to_string(),
                b["data"]["error"].as_str().unwrap_or("").to_owned(),
            ));
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let (done, err) = match &terminal {
        Some((st, e)) => (st == "completed", e.clone()),
        None => (false, "240s 未到终态".into()),
    };
    report.assert_hard(
        "dev/start 任务 completed（跳过编译不破坏编排）",
        done,
        format!("terminal={terminal:?}, err: {err}"),
    );

    // 三分派黑盒断言（marker 探针）
    let go_built = file_exists(&env, &app, user, "go-svc/built-go.marker").await;
    report.assert_hard(
        "未配 devrun 的服务回落 [build].command（go-svc marker 落盘）",
        go_built == Some(true),
        format!("resolve-file go-svc/built-go.marker → {go_built:?}"),
    );
    let dev_built = file_exists(&env, &app, user, "dev-svc/built-dev.marker").await;
    report.assert_hard(
        "只配 devrun 的服务跳过编译（dev-svc marker 不落盘）",
        dev_built == Some(false),
        format!("resolve-file dev-svc/built-dev.marker → {dev_built:?}"),
    );

    // dev/list：port=9080 + pid>0（编排存活证据）
    let resp = env
        .http
        .get(format!(
            "{}/api/v1/userapp/dev/list?app_id={app}&user_id={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(15))
        .header("X-App-Id", &app)
        .send()
        .await
        .expect("dev list");
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let listed = body["data"]["list"].as_array().is_some_and(|arr| {
        arr.iter().any(|p| {
            p["port"].as_u64() == Some(9080) && p["pid"].as_u64().is_some_and(|pid| pid > 0)
        })
    });
    report.assert_hard(
        "dev/list → port=9080 + pid>0",
        listed,
        format!("body 截断: {}", trunc(&body, 150)),
    );

    // dev/stop → Stopped（收尾，防 builder 残留进程族）
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/dev/stop", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user}))
        .send()
        .await
        .expect("dev stop");
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "dev/stop → Stopped",
        body["data"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Stopped")),
        format!("body 截断: {}", trunc(&body, 120)),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}
