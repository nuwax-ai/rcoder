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
//! - pnpm devbuild 无 lockfile（app 110 事故回归）：--no-frozen-lockfile 安装
//!   成功并生成 lockfile、devrun 可用；--frozen-lockfile + 过期 lockfile →
//!   任务 failed（错误如实传播）

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

/// 场景内唯一 app_id（与 compose_userapp_dev 同款；复合键改造后 [a-z0-9]
/// 禁 `-`、上限 22——前缀字母段对齐 logs 族命名约束）。
fn scoped_app(env: &Env, tag: &str) -> String {
    let short_tag: String = tag
        .split('-')
        .filter_map(|part| part.chars().next())
        .collect();
    let raw = format!(
        "appe2ebr{}p{}{}",
        &env.run_tag.replace('_', "").to_lowercase()[..10],
        std::process::id() % 1000,
        short_tag.to_lowercase()
    );
    raw.chars().take(22).collect()
}

fn cleanup_builder(app_id: &str) {
    // 复合键后容器名含实例 user 段（本文件场景 owner 恒为 e2e-br-user）
    if let Err(error) =
        rcoder_e2e::common::resources::cleanup_container(&format!("rcoder-app-builder-{app_id}"))
    {
        eprintln!("owned builder cleanup failed: {error}");
    }
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
        let Ok(resp) = resp else {
            rcoder_e2e::common::resources::register_builder_attempt(app_id, false)
                .expect("register uncertain builder creation");
            continue;
        };
        status = resp.status();
        body = resp.json().await.unwrap_or(Value::Null);
        rcoder_e2e::common::resources::register_builder_attempt(
            app_id,
            status.is_success() && http_ok(&body),
        )
        .expect("register builder creation identity");
        if status.is_success() && http_ok(&body) {
            break;
        }
        if !rcoder_e2e::common::retry::ensure_transient(status, &body) {
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
    let go_manifest = "schema_version = 1\n\n[project]\nservice_id = \"go-svc\"\nname = \"Go API\"\ntype = \"go\"\n\n[build]\ncommand = [\"sh\", \"-c\", \"touch built-go.marker\"]\nartifact = \"artifact.zip\"\n\n[run]\ncommand = [\"sh\", \"-c\", \"echo Q10_OLD_GO > ready-go && exec python3 -m http.server $PORT --bind 0.0.0.0\"]\n\n[health]\nreadiness_path = \"/ready-go\"\n\n[proxy]\npath = \"/api/go/\"\nstrip_prefix = true\n";
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

    // Q10: start still builds when an existing process is healthy. A failed build
    // must not become an idempotent success, nor stop the previously running app.
    let failing_manifest =
        format!("{dev_manifest}\n[devbuild]\ncommand = [\"sh\", \"-c\", \"exit 37\"]\n");
    let changed = env.http.post(format!("{}/api/v1/userapp/generate-file", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user, "file_name": "dev-svc/project.manifest.toml", "content": failing_manifest}))
        .send().await.expect("write failing devbuild");
    let changed: Value = changed.json().await.expect("file write response");
    let fixture_installed = changed["success"] == true || http_ok(&changed);
    report.assert_hard(
        "Q10 failing build fixture installed",
        fixture_installed,
        trunc(&changed, 200),
    );
    if !fixture_installed {
        cleanup_builder(&app);
        assert_hard_all(report).await;
        return;
    }
    let second: Value = env
        .http
        .post(format!("{}/api/v1/userapp/dev/start", env.rcoder))
        .timeout(Duration::from_secs(30))
        .json(&json!({"app_id": app, "user_id": user}))
        .send()
        .await
        .expect("second start")
        .json()
        .await
        .expect("start response");
    let second_id = second["data"]["task_id"]
        .as_str()
        .expect("second task identity");
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut second_terminal = Value::Null;
    while Instant::now() < deadline {
        let snapshot: Value = env
            .http
            .get(format!(
                "{}/api/v1/userapp/tasks/{second_id}?app_id={app}&user_id={user}",
                env.rcoder
            ))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .expect("second task poll")
            .json()
            .await
            .expect("task JSON");
        if matches!(
            snapshot["data"]["status"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            second_terminal = snapshot;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    report.assert_hard(
        "Q10 failed build cannot become already-running success",
        second_terminal["data"]["status"] == "failed",
        trunc(&second_terminal, 300),
    );
    let after: Value = env
        .http
        .get(format!(
            "{}/api/v1/userapp/dev/list?app_id={app}&user_id={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("list after failure")
        .json()
        .await
        .expect("list JSON");
    report.assert_hard(
        "Q10 previous process preserved after build failure",
        listed
            && body["data"]["list"]
                .as_array()
                .and_then(|items| items.iter().find(|item| item["port"] == 9080))
                .map(|item| &item["pid"])
                == after["data"]["list"]
                    .as_array()
                    .and_then(|items| items.iter().find(|item| item["port"] == 9080))
                    .map(|item| &item["pid"]),
        trunc(&after, 200),
    );

    let pingora =
        std::env::var("E2E_PINGORA_URL").unwrap_or_else(|_| "http://127.0.0.1:8089".into());
    let served = env
        .http
        .get(format!(
            "{pingora}/api/v1/userapp/proxy/app/dev/{user}/{app}/api/go/ready-go"
        ))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("old app proxy response");
    let status = served.status();
    let content = served.text().await.expect("old app content");
    report.assert_hard(
        "Q10 previous content remains healthy after build failure",
        status.is_success() && content.trim() == "Q10_OLD_GO",
        format!("HTTP {status}, {content}"),
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

// ============================================================
// 场景 4：pnpm devbuild 无 lockfile（app 110 事故回归）——
//          [devbuild] `pnpm install --no-frozen-lockfile` 在无 lockfile 的
//          真实 builder 容器内安装成功并生成 lockfile、devrun 可用；
//          改回 `--frozen-lockfile` 且 lockfile 过期后 dev build 失败
//          （事故形态），错误如实传播且不杀已有进程。
// ============================================================
/// dev/start 并轮询到终态，返回任务 data 快照（受理失败/超预算 → None + hard 红）。
async fn dev_start_to_terminal(
    env: &Env,
    report: &JsonlReporter,
    app: &str,
    user: &str,
    budget: Duration,
) -> Option<Value> {
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/dev/start", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", app)
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
            && matches!(st, "completed" | "failed" | "cancelled")
        {
            report.diagnostic(
                "dev/start 到达终态",
                &format!("{:.0}s, status={st}", t0.elapsed().as_secs_f64()),
                b["data"]["error"].as_str().unwrap_or(""),
            );
            return Some(b["data"].clone());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    report.assert_hard(
        "dev/start 到达终态（预算内不挂死）",
        false,
        format!("{:.0}s 未到终态", t0.elapsed().as_secs_f64()),
    );
    None
}

/// generate-file 覆写 workspace 内相对路径文件（Q10 同款）。
async fn overwrite_file(env: &Env, app: &str, user: &str, file_name: &str, content: &str) -> Value {
    env.http
        .post(format!("{}/api/v1/userapp/generate-file", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", app)
        .json(&json!({
            "app_id": app,
            "user_id": user,
            "file_name": file_name,
            "content": content,
        }))
        .send()
        .await
        .expect("generate-file")
        .json()
        .await
        .expect("file write response")
}

#[tokio::test]
async fn userapp_devbuild_no_lockfile_pnpm_install() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_devbuild_rules";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "pn");
    let user = "e2e-br-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 单服务 node 工程：无 pnpm-lock.yaml + 本地 file: 依赖（离线安装，不碰
    // registry）。运行时镜像预装 node + pnpm@10。devbuild 为修复后命令；devrun
    // 为最小 node http 服务。
    let ws_manifest = "schema_version = 1\n\n[workspace]\nname = \"e2e-br-pnpm\"\n";
    let pnpm_manifest = "schema_version = 1\n\n[project]\nservice_id = \"pnpm-svc\"\nname = \"Pnpm Dev\"\ntype = \"node\"\n\n[build]\ncommand = [\"sh\", \"-c\", \"touch built-pnpm.marker\"]\nartifact = \"artifact.zip\"\n\n[run]\ncommand = [\"sh\", \"-c\", \"exec python3 -m http.server $PORT --bind 0.0.0.0\"]\n\n[health]\nreadiness_path = \"/ready-pnpm\"\n\n[devbuild]\ncommand = [\"sh\", \"-c\", \"pnpm install --no-frozen-lockfile\"]\n\n[devrun]\ncommand = [\"sh\", \"-c\", \"exec node server.js\"]\n\n[proxy]\npath = \"/api/pnpm/\"\nstrip_prefix = true\n";
    let server_js = "const http = require('http');\nhttp.createServer(function (req, res) { res.writeHead(200, {'Content-Type': 'text/plain'}); res.end('PNPM_DEV_OK'); }).listen(process.env.PORT || 3000, '0.0.0.0');\n";
    let pkg_json = "{\n  \"name\": \"pnpm-dev-fixture\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": { \"dep-a\": \"file:./vendor/dep-a\" }\n}\n";
    let dep_pkg = "{ \"name\": \"dep-a\", \"version\": \"1.0.0\" }\n";
    if !init_zip_workspace(
        &env,
        &report,
        &app,
        user,
        &[
            ("workspace.manifest.toml", ws_manifest),
            ("pnpm-svc/project.manifest.toml", pnpm_manifest),
            ("pnpm-svc/server.js", server_js),
            ("pnpm-svc/package.json", pkg_json),
            ("pnpm-svc/vendor/dep-a/package.json", dep_pkg),
        ],
    )
    .await
    {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 修复后链路：无 lockfile → 安装成功 + lockfile 生成 + 编排完成（真实 pnpm）
    let terminal = dev_start_to_terminal(&env, &report, &app, user, Duration::from_secs(240)).await;
    report.assert_hard(
        "无 lockfile dev/start completed（--no-frozen-lockfile 安装成功）",
        terminal
            .as_ref()
            .is_some_and(|d| d["status"] == "completed"),
        format!("terminal={}", trunc(&terminal.unwrap_or(Value::Null), 200)),
    );

    let lockfile = file_exists(&env, &app, user, "pnpm-svc/pnpm-lock.yaml").await;
    report.assert_hard(
        "devbuild 生成 pnpm-lock.yaml（修复前此处 ERR_PNPM_NO_LOCKFILE 失败）",
        lockfile == Some(true),
        format!("resolve-file pnpm-svc/pnpm-lock.yaml → {lockfile:?}"),
    );

    // 编排存活 + devrun 真实服务内容（经 pingora 代理）
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
        arr.iter()
            .any(|p| p["pid"].as_u64().is_some_and(|pid| pid > 0))
    });
    report.assert_hard(
        "dev/list → pid>0（devrun 存活）",
        listed,
        format!("body 截断: {}", trunc(&body, 150)),
    );
    let pingora =
        std::env::var("E2E_PINGORA_URL").unwrap_or_else(|_| "http://127.0.0.1:8089".into());
    let served = env
        .http
        .get(format!(
            "{pingora}/api/v1/userapp/proxy/app/dev/{user}/{app}/api/pnpm/ready-pnpm"
        ))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .expect("devrun proxy response");
    let served_status = served.status();
    let served_content = served.text().await.expect("devrun content");
    report.assert_hard(
        "devrun 服务内容经代理可达（node server.js）",
        served_status.is_success() && served_content.trim() == "PNPM_DEV_OK",
        format!("HTTP {served_status}, {served_content}"),
    );

    // 事故反例：devbuild 改回 --frozen-lockfile 且依赖声明变更使 lockfile 过期
    // → 安装失败（ERR_PNPM_OUTDATED_LOCKFILE 形态）、任务 failed、旧进程保留
    let dep_b_pkg = "{ \"name\": \"dep-b\", \"version\": \"1.0.0\" }\n";
    let pkg_json_stale = "{\n  \"name\": \"pnpm-dev-fixture\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": { \"dep-a\": \"file:./vendor/dep-a\", \"dep-b\": \"file:./vendor/dep-b\" }\n}\n";
    let pnpm_manifest_frozen = "schema_version = 1\n\n[project]\nservice_id = \"pnpm-svc\"\nname = \"Pnpm Dev\"\ntype = \"node\"\n\n[build]\ncommand = [\"sh\", \"-c\", \"touch built-pnpm.marker\"]\nartifact = \"artifact.zip\"\n\n[run]\ncommand = [\"sh\", \"-c\", \"exec python3 -m http.server $PORT --bind 0.0.0.0\"]\n\n[health]\nreadiness_path = \"/ready-pnpm\"\n\n[devbuild]\ncommand = [\"sh\", \"-c\", \"pnpm install --frozen-lockfile\"]\n\n[devrun]\ncommand = [\"sh\", \"-c\", \"exec node server.js\"]\n\n[proxy]\npath = \"/api/pnpm/\"\nstrip_prefix = true\n";
    for (file_name, content) in [
        ("pnpm-svc/vendor/dep-b/package.json", dep_b_pkg),
        ("pnpm-svc/package.json", pkg_json_stale),
        ("pnpm-svc/project.manifest.toml", pnpm_manifest_frozen),
    ] {
        let changed = overwrite_file(&env, &app, user, file_name, content).await;
        let installed = changed["success"] == true || http_ok(&changed);
        report.assert_hard(
            "事故反例 fixture 写入（frozen + 过期 lockfile）",
            installed,
            trunc(&changed, 200),
        );
        if !installed {
            assert_hard_all(report).await;
            cleanup_builder(&app);
            return;
        }
    }
    let frozen_terminal =
        dev_start_to_terminal(&env, &report, &app, user, Duration::from_secs(240)).await;
    let frozen_failed = frozen_terminal
        .as_ref()
        .is_some_and(|d| d["status"] == "failed");
    let frozen_error = frozen_terminal
        .as_ref()
        .and_then(|d| d["error"].as_str())
        .unwrap_or("")
        .to_owned();
    report.assert_hard(
        "frozen + 过期 lockfile → 任务 failed（错误如实传播）",
        frozen_failed && frozen_error.contains("dev build failed"),
        format!(
            "terminal={}",
            trunc(&frozen_terminal.unwrap_or(Value::Null), 300)
        ),
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

// ============================================================
// 场景：prod build 缺 lockfile 自愈（app-171 事故回归）
// 平台导出链过滤 pnpm-lock.yaml（既定设计）+ 存量模板 build 脚本
// --frozen-lockfile → ERR_PNPM_NO_LOCKFILE。自愈：失败码触发 pnpm
// install 生成 lockfile 后重试一次构建。
// ============================================================
#[tokio::test]
async fn userapp_build_no_lockfile_pnpm_install() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_build_no_lockfile";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "bn");
    let user = "e2e-br-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 单服务 node 工程：无 pnpm-lock.yaml + 本地 file: 依赖（离线安装）。
    // [build] 刻意用旧模板形态 --frozen-lockfile（app-171 事故现场形状）。
    let ws_manifest = "schema_version = 1\n\n[workspace]\nname = \"e2e-br-build-heal\"\n";
    let frozen_manifest = "schema_version = 1\n\n[project]\nservice_id = \"build-heal-svc\"\nname = \"Build Heal\"\ntype = \"node\"\n\n[build]\ncommand = [\"sh\", \"-c\", \"pnpm install --frozen-lockfile && mkdir -p dist && echo ok > dist/index.html\"]\nartifact = \"dist\"\n\n[proxy]\npath = \"/api/heal/\"\nstrip_prefix = true\n";
    let pkg_json = "{\n  \"name\": \"build-heal-fixture\",\n  \"version\": \"1.0.0\",\n  \"dependencies\": { \"dep-a\": \"file:./vendor/dep-a\" }\n}\n";
    let dep_pkg = "{ \"name\": \"dep-a\", \"version\": \"1.0.0\" }\n";
    if !init_zip_workspace(
        &env,
        &report,
        &app,
        user,
        &[
            ("workspace.manifest.toml", ws_manifest),
            ("build-heal-svc/project.manifest.toml", frozen_manifest),
            ("build-heal-svc/package.json", pkg_json),
            ("build-heal-svc/vendor/dep-a/package.json", dep_pkg),
        ],
    )
    .await
    {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 正例：frozen + 无 lockfile → 自愈（install 生成 lockfile + 重试一次）→ completed
    let terminal = build_to_terminal(&env, &report, &app, user, Duration::from_secs(300)).await;
    let completed = terminal
        .as_ref()
        .is_some_and(|d| d["status"] == "completed");
    report.assert_hard(
        "无 lockfile build completed（frozen 失败→pnpm install→重试成功）",
        completed,
        format!("terminal={}", trunc(&terminal.unwrap_or(Value::Null), 240)),
    );
    let lockfile = file_exists(&env, &app, user, "build-heal-svc/pnpm-lock.yaml").await;
    report.assert_hard(
        "build 自愈生成 pnpm-lock.yaml（修复前此处 ERR_PNPM_NO_LOCKFILE 失败）",
        lockfile == Some(true),
        format!("resolve-file build-heal-svc/pnpm-lock.yaml → {lockfile:?}"),
    );

    // 反例：非该码失败不得触发自愈（错误如实传播，无 self-heal 上下文）。
    let plain_fail_manifest = frozen_manifest.replace(
        "pnpm install --frozen-lockfile && mkdir -p dist && echo ok > dist/index.html",
        "exit 7",
    );
    assert_ne!(
        plain_fail_manifest, frozen_manifest,
        "反例 manifest 必须生效"
    );
    overwrite_file(
        &env,
        &app,
        user,
        "build-heal-svc/project.manifest.toml",
        &plain_fail_manifest,
    )
    .await;
    let terminal = build_to_terminal(&env, &report, &app, user, Duration::from_secs(120)).await;
    let failed_honestly = terminal.as_ref().is_some_and(|d| {
        d["status"] == "failed"
            && d["error"]
                .as_str()
                .is_some_and(|e| e.contains("exited non-zero"))
            && !e_no_self_heal(d)
    });
    report.assert_hard(
        "非 ERR_PNPM_NO_LOCKFILE 失败不自愈（错误如实传播）",
        failed_honestly,
        format!("terminal={}", trunc(&terminal.unwrap_or(Value::Null), 240)),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

fn e_no_self_heal(data: &Value) -> bool {
    data["error"]
        .as_str()
        .is_some_and(|message| message.contains("self-heal"))
}
