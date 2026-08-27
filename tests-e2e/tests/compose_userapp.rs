//! compose 环境 UserApp 面回归（rcoder publish 任务体系删除面 + file-server 构建链终态）。
//!
//! 运行: `make test-e2e-compose` 或
//! `cargo test -p rcoder-e2e --test compose_userapp -- --test-threads=1`
//! （Python 对照版：tests/userapp_compose_regression.py）

use std::time::{Duration, Instant};

use rcoder_e2e::common::Env;
use rcoder_e2e::common::report::JsonlReporter;
use serde_json::{Value, json};

/// HttpResult 包装的成功判定（success 字段 serde skip，判定用 code == "0000"）。
fn http_ok(body: &Value) -> bool {
    body["code"].as_str() == Some("0000")
}

async fn post_json(env: &Env, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let resp = env
        .http
        .post(format!("{}{path}", env.rcoder))
        .timeout(Duration::from_secs(60))
        .json(&body)
        .send()
        .await
        .expect("http post");
    let status = resp.status();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// 显式清理 build 触发的 builder 容器（rcoder-app-builder-<app_id>；
/// TestUserGuard 只清 agent-runner 前缀，builder 需场景自理）。
fn cleanup_builder(app_id: &str) {
    let name = format!("rcoder-app-builder-{app_id}");
    std::process::Command::new("docker")
        .args(["rm", "-f", &name])
        .output()
        .ok();
}

/// rcoder 侧 publish 任务体系已删（构建链收敛为 file-server `/api/userapp/*`
/// 接口族 + start(url) 部署）：删除面锁——旧路径不得再挂路由。
async fn test_publish_endpoints_removed(env: &Env, report: &JsonlReporter) {
    let (s1, _) = post_json(env, "/api/v1/apps/publish/tasks/query", json!({})).await;
    report.assert_hard(
        "POST /api/v1/apps/publish/tasks/query 已删除（404）",
        s1.as_u16() == 404,
        format!("HTTP {s1}"),
    );
    let (s2, _) = post_json(env, "/api/v1/apps/app-any/build", json!({})).await;
    report.assert_hard(
        "POST /api/v1/apps/{app_id}/build 已删除（404）",
        s2.as_u16() == 404,
        format!("HTTP {s2}"),
    );
}

/// file-server build 入口校验：缺转发 header `X-App-Id` → 400 快速失败（不挂起、
/// 无容器副作用）。body 内 app_id 格式的严格校验是 TS 对齐语义（file-server 侧
/// 仅 not_blank），随 nuwax-file-server 改造对齐，不在本测试范围。
async fn test_build_identifier_validation(env: &Env, report: &JsonlReporter) {
    let t0 = Instant::now();
    let resp = env
        .http
        .post(format!("{}/api/userapp/build", env.rcoder))
        .timeout(Duration::from_secs(30))
        .json(&json!({"app_id": "app-e2e-noheader", "user_id": "e2e-user"}))
        .send()
        .await
        .expect("http post");
    let status = resp.status();
    let elapsed = t0.elapsed();
    report.assert_hard(
        "build 缺 X-App-Id 返回 400 且不挂起",
        status.as_u16() == 400 && elapsed < Duration::from_secs(5),
        format!("HTTP {status}, {:.1}s", elapsed.as_secs_f64()),
    );
}

/// file-server 构建链终态收敛：create-workspace → build 受理（task_id + artifact_path）
/// → tasks 轮询到终态（不挂死）→ static 按 app 直下（completed=200 / failed=404）。
async fn test_build_reaches_terminal(env: &Env, report: &JsonlReporter) {
    // ident 唯一化：run_tag(秒级)+pid——不同测试进程同秒启动也不撞名
    let ident = format!(
        "app-e2e-rs-{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let guard_app = ident.clone();

    // 前置：创建项目工作区（ensure 开发容器 + 幂等建目录；缺失时容器内 build
    // fail fast "workspace not found"）
    let (ws_status, ws_body) = post_json(
        env,
        "/api/userapp/workspace",
        json!({"app_id": ident, "user_id": "e2e-user"}),
    )
    .await;
    report.diagnostic(
        "create-workspace 前置（ensure 开发容器+建目录）",
        &format!("{}", ws_status.as_u16()),
        &format!("HTTP {ws_status}, body 截断: {}", trunc(&ws_body, 120)),
    );

    let resp = env
        .http
        .post(format!("{}/api/userapp/build", env.rcoder))
        .timeout(Duration::from_secs(60))
        .header("X-App-Id", &ident)
        .json(&json!({"app_id": ident, "user_id": "e2e-user"}))
        .send()
        .await
        .expect("http post");
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
    // task_id 非空一并校验：键名漂移（读错字段得空串）会让后续轮询 404 循环
    // 到超时，表象是"疑似挂死"——在受理处即拦截。artifact_path 为受理即有的
    // 信息字段（快照同源）
    let ok = status.is_success()
        && http_ok(&body)
        && !task_id.is_empty()
        && artifact_path.starts_with("builds/workspace-package-");
    report.assert_hard(
        "build 受理（200 + task_id + artifact_path 预生成）",
        ok,
        format!("HTTP {status}, body 截断: {}", trunc(&body, 120)),
    );
    if !ok {
        cleanup_builder(&guard_app);
        return;
    }

    // 轮询到终态（failed/cancelled/completed）；workspace 无真实项目 → 业务性 failed
    let t0 = Instant::now();
    let mut terminal: Option<String> = None;
    while t0.elapsed() < Duration::from_secs(180) {
        let resp = env
            .http
            .get(format!("{}/api/userapp/tasks/{task_id}", env.rcoder))
            .timeout(Duration::from_secs(10))
            .header("X-App-Id", &guard_app)
            .send()
            .await;
        if let Ok(r) = resp
            && r.status().is_success()
            && let Ok(b) = r.json::<Value>().await
        {
            let status = b["data"]["status"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase();
            if matches!(status.as_str(), "failed" | "cancelled" | "completed") {
                terminal = Some(status);
                report.chat_request(rcoder_e2e::common::report::ChatTrace {
                    phase: "terminal_poll",
                    url: &env.rcoder,
                    ok: true,
                    request_sanitized: json!({"task_id": task_id}),
                    response: Some(&b["data"]),
                    error: None,
                    elapsed_ms: t0.elapsed().as_millis(),
                });
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    report.assert_hard(
        "build 任务 180s 内到达终态（不挂死）",
        terminal.is_some(),
        match &terminal {
            Some(s) => format!("status={s}, {:.0}s", t0.elapsed().as_secs_f64()),
            None => "180s 未到终态（疑似挂死）".to_owned(),
        },
    );

    // static 按 app 直下：两段路径（不传文件名）；无 completed 产物（failed）→ 404，
    // completed → 200。本场景 workspace 无真实项目，预期 failed + 404
    let static_resp = env
        .http
        .get(format!("{}/api/userapp/static/{guard_app}", env.rcoder))
        .timeout(Duration::from_secs(15))
        .header("X-App-Id", &guard_app)
        .send()
        .await;
    let expected_ok = terminal.as_deref() == Some("completed");
    let static_ok = match static_resp {
        Ok(r) => {
            let code = r.status().as_u16();
            expected_ok == (code == 200)
        }
        Err(_) => false,
    };
    cleanup_builder(&guard_app);
    report.assert_hard(
        "static 按 app 直下（两段）与终态一致（completed=200 / 其他=404）",
        static_ok,
        format!("terminal={terminal:?}, expected_200={expected_ok}"),
    );
}

/// start 无 url 三态语义：不存在 + 缺 user_id → 400；不存在 + 带 user_id → 创建
/// 空容器（200，基础设施形态）；restart 无 url 对不存在 app → 仍 404（重启不创建）。
async fn test_start_without_app_semantics(env: &Env, report: &JsonlReporter) {
    let suffix = format!(
        "{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );

    // ① 不存在 + 缺 user_id → 400（数据卷分区依赖）
    let (s1, b1) = post_json(
        env,
        &format!("/api/v1/apps/app-e2e-nokey-{suffix}/start"),
        json!({}),
    )
    .await;
    report.assert_hard(
        "start 无 url 创建空容器缺 user_id → 400",
        s1.as_u16() == 400,
        format!("HTTP {s1}, {}", trunc(&b1, 100)),
    );

    // ② 不存在 + 带 user_id → 200 创建空容器（Running，无部署内容）
    let app_id = format!("app-e2e-empty-{suffix}");
    let (s2, b2) = post_json(
        env,
        &format!("/api/v1/apps/{app_id}/start"),
        json!({"user_id": "e2e-user"}),
    )
    .await;
    let created = s2.is_success() && http_ok(&b2);
    report.assert_hard(
        "start 无 url 对不存在 app 创建空容器（200）",
        created,
        format!("HTTP {s2}, body 截断: {}", trunc(&b2, 150)),
    );
    if created {
        let status = b2["data"]["status"].as_str().unwrap_or("");
        report.assert_hard(
            "空容器状态 Running（基础设施形态就绪）",
            status == "running",
            format!("status={status}"),
        );
        // cleanup：删除 app（purge 连数据卷一起清，防 compose 环境残留）
        drop(
            post_json(
                env,
                &format!("/api/v1/apps/{app_id}/delete"),
                json!({"purge": true}),
            )
            .await,
        );
    }

    // ③ restart 无 url 对不存在 app → 404（重启语义不创建）
    let (s3, _) = post_json(
        env,
        &format!("/api/v1/apps/app-e2e-norestart-{suffix}/restart"),
        json!({}),
    )
    .await;
    report.assert_hard(
        "restart 无 url 对不存在的 app → 404（不创建）",
        s3.as_u16() == 404,
        format!("HTTP {s3}"),
    );
}

/// env 维度文件/存储链路（`{app_id}/{env}/*` 八接口新形态的 dev 侧回归锚）：
/// create-workspace → upload(dev) → files(dev) 断言 → storage(dev) 查询 →
/// storage/{env}/query 清单 → 非法 env 400 → destroy(dev) 回收 → exists 复查。
async fn test_env_scoped_files_and_storage(env: &Env, report: &JsonlReporter) {
    let ident = format!(
        "app-e2e-env-{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let guard_app = ident.clone();

    // 前置：建 workspace（ensure 开发容器 + 幂等建目录——upload/dev 链路的载体）
    let (ws_s, ws_b) = post_json(
        env,
        "/api/userapp/workspace",
        json!({"app_id": ident, "user_id": "e2e-user"}),
    )
    .await;
    let ws_ok = ws_s.is_success() && http_ok(&ws_b);
    report.assert_hard(
        "create-workspace 前置（dev 链路载体）",
        ws_ok,
        format!("HTTP {ws_s}, body 截断: {}", trunc(&ws_b, 120)),
    );
    if !ws_ok {
        cleanup_builder(&guard_app);
        return;
    }

    // upload(dev)：multipart 单文件直传（target 相对开发容器 workspace 根）
    let part = reqwest::multipart::Part::bytes(b"e2e-dev-upload").file_name("hello.txt");
    let form = reqwest::multipart::Form::new()
        .text("target", "hello.txt")
        .part("file", part);
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/{ident}/dev/upload", env.rcoder))
        .timeout(Duration::from_secs(120))
        .multipart(form)
        .send()
        .await
        .expect("upload(dev)");
    let up_status = resp.status();
    let up_body: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "upload(dev) 受理（200 + data.file_path 含上传名）",
        up_status.is_success()
            && http_ok(&up_body)
            && up_body["data"]["file_path"]
                .as_str()
                .is_some_and(|p| p.contains("hello.txt")),
        format!("HTTP {up_status}, body 截断: {}", trunc(&up_body, 150)),
    );

    // files(dev)：workspace 根列表含上传文件
    let resp = env
        .http
        .get(format!("{}/api/v1/userapp/{ident}/dev/files", env.rcoder))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("files(dev)");
    let ls_status = resp.status();
    let ls_body: Value = resp.json().await.unwrap_or(Value::Null);
    let listed = ls_status.is_success()
        && http_ok(&ls_body)
        && ls_body["data"]
            .as_array()
            .is_some_and(|files| files.iter().any(|f| f["path"] == "hello.txt"));
    report.assert_hard(
        "files(dev) 列表含上传文件（env=dev 根=workspace）",
        listed,
        format!("HTTP {ls_status}, body 截断: {}", trunc(&ls_body, 150)),
    );

    // storage(dev)：workspace 已建 → exists=true
    let (st_s, st_b) = get_json(env, &format!("/api/v1/userapp/{ident}/dev/storage")).await;
    report.assert_hard(
        "storage(dev) exists=true（workspace 就绪）",
        st_s.is_success() && http_ok(&st_b) && st_b["data"]["exists"] == true,
        format!("HTTP {st_s}, body 截断: {}", trunc(&st_b, 150)),
    );

    // storage/{env}/query：dev 清单含该 app
    let (q_s, q_b) = post_json(
        env,
        "/api/v1/userapp/storage/dev/query",
        json!({"page": 1, "page_size": 50, "filters": {"app_ids": [ident]}}),
    )
    .await;
    report.assert_hard(
        "storage/dev/query 清单含本 app",
        q_s.is_success()
            && http_ok(&q_b)
            && q_b["data"]["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|i| i["app_id"] == ident)),
        format!("HTTP {q_s}, body 截断: {}", trunc(&q_b, 150)),
    );

    // env 必填校验：非法值 400
    let (bad_s, _) = get_json(env, &format!("/api/v1/userapp/{ident}/staging/storage")).await;
    report.assert_hard(
        "非法 env → 400（必填显式，无缺省）",
        bad_s.as_u16() == 400,
        format!("HTTP {bad_s}"),
    );

    // destroy(dev)：confirm=app_id → 回收整个开发环境（容器+卷+目录）
    let (d_s, d_b) = post_json(
        env,
        &format!("/api/v1/userapp/{ident}/dev/storage/destroy"),
        json!({"confirm": ident}),
    )
    .await;
    report.assert_hard(
        "storage/dev/destroy 回收开发环境",
        d_s.is_success() && http_ok(&d_b),
        format!("HTTP {d_s}, body 截断: {}", trunc(&d_b, 150)),
    );

    // 回收后复查：exists=false
    let (re_s, re_b) = get_json(env, &format!("/api/v1/userapp/{ident}/dev/storage")).await;
    report.assert_hard(
        "destroy 后 storage(dev) exists=false",
        re_s.is_success() && http_ok(&re_b) && re_b["data"]["exists"] == false,
        format!("HTTP {re_s}, body 截断: {}", trunc(&re_b, 150)),
    );

    cleanup_builder(&guard_app);
}

/// GET + HttpResult 解析（本文件 env 场景共用）。
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

fn trunc(v: &Value, n: usize) -> String {
    let s = v.to_string();
    s.chars().take(n).collect()
}

#[tokio::test]
async fn userapp_compose_regression() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let scenario = "userapp_compose";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    test_publish_endpoints_removed(&env, &report).await;
    test_build_identifier_validation(&env, &report).await;
    test_build_reaches_terminal(&env, &report).await;
    test_start_without_app_semantics(&env, &report).await;
    test_env_scoped_files_and_storage(&env, &report).await;

    let path = report.path.display().to_string();
    assert!(report.finish(), "场景失败：断言明细见 {path}");
}
