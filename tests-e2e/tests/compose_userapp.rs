//! compose 环境 Userapp 面回归（rcoder publish 任务体系删除面 + file-server 构建链终态）。
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
        .post(format!("{}/api/v1/userapp/build", env.rcoder))
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
        "/api/v1/userapp/workspace",
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
        .post(format!("{}/api/v1/userapp/build", env.rcoder))
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
            .get(format!(
                "{}/api/v1/userapp/tasks/{task_id}?app_id={guard_app}&user_id=e2e-user",
                env.rcoder
            ))
            .timeout(Duration::from_secs(10))
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
        .get(format!(
            "{}/api/v1/userapp/static/{guard_app}?user_id=e2e-user",
            env.rcoder
        ))
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

    // ① 不存在 + 缺 user_id → 422（serde 提取层拒缺必填字段——user_id 必填化
    //    后 Json 反序列化先于 garde 校验，axum 裸 422；带提示 missing field）
    let (s1, b1) = post_json(
        env,
        &format!("/api/v1/userapp/app-e2e-nokey-{suffix}/start"),
        json!({}),
    )
    .await;
    report.assert_hard(
        "start 无 url 缺 user_id → 422（必填字段提取层拒绝）",
        s1.as_u16() == 422,
        format!("HTTP {s1}, {}", trunc(&b1, 100)),
    );

    // ② 不存在 + 带 user_id → 200 创建空容器（Running，无部署内容）
    let app_id = format!("app-e2e-empty-{suffix}");
    let (s2, b2) = post_json(
        env,
        &format!("/api/v1/userapp/{app_id}/start"),
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
        // 轮询到 running：Docker 立即就绪；K8s 异步（Pod 探针等 app-cli
        // /health，start 响应时可能仍 starting——双环境统一轮询写法）
        let mut st = b2["data"]["status"].as_str().unwrap_or("").to_string();
        let t0 = Instant::now();
        while st != "running" && t0.elapsed() < Duration::from_secs(120) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let resp = env
                .http
                .get(format!(
                    "{}/api/v1/userapp/{app_id}?user_id=e2e-user",
                    env.rcoder
                ))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
            if let Ok(r) = resp
                && let Ok(v) = r.json::<Value>().await
            {
                st = v["data"]["status"].as_str().unwrap_or("").to_string();
            }
        }
        report.assert_hard(
            "空容器状态 Running（基础设施形态就绪；120s 轮询兼容 K8s 异步）",
            st == "running",
            format!("status={st}"),
        );
        // cleanup：删除 app（purge 连数据卷一起清，防 compose 环境残留）
        drop(
            post_json(
                env,
                &format!("/api/v1/userapp/{app_id}/prod/delete"),
                json!({"user_id": "e2e-user", "purge": true}),
            )
            .await,
        );
    }

    // ③ restart 无 url 对不存在 app → 404（重启语义不创建；user_id 必填带到业务层）
    let (s3, _) = post_json(
        env,
        &format!("/api/v1/userapp/app-e2e-norestart-{suffix}/restart"),
        json!({"user_id": "e2e-user"}),
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
        "/api/v1/userapp/workspace",
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
        .text("user_id", "e2e-user")
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
        .get(format!(
            "{}/api/v1/userapp/{ident}/dev/files?user_id=e2e-user",
            env.rcoder
        ))
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
    let (st_s, st_b) = get_json(
        env,
        &format!("/api/v1/userapp/{ident}/dev/storage?user_id=e2e-user"),
    )
    .await;
    report.assert_hard(
        "storage(dev) exists=true（workspace 就绪）",
        st_s.is_success() && http_ok(&st_b) && st_b["data"]["exists"] == true,
        format!("HTTP {st_s}, body 截断: {}", trunc(&st_b, 150)),
    );

    // storage/{env}/query：dev 清单含该 app
    let (q_s, q_b) = post_json(
        env,
        "/api/v1/userapp/storage/dev/query",
        json!({"user_id": "e2e-user", "page": 1, "page_size": 50, "filters": {"app_ids": [ident]}}),
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
        json!({"user_id": "e2e-user", "confirm": ident}),
    )
    .await;
    report.assert_hard(
        "storage/dev/destroy 回收开发环境",
        d_s.is_success() && http_ok(&d_b),
        format!("HTTP {d_s}, body 截断: {}", trunc(&d_b, 150)),
    );

    // 回收后复查：exists=false
    let (re_s, re_b) = get_json(
        env,
        &format!("/api/v1/userapp/{ident}/dev/storage?user_id=e2e-user"),
    )
    .await;
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

/// dev 日志尾读 + 应用列表双接口（观测面此前 e2e 零覆盖）：
/// files-update 写 `.logs/` 探针文件 → get-logs 尾读断言回显；
/// query（owner 过滤）/runtime（运行时列表）双列表接口。
async fn test_dev_logs_and_listing(env: &Env, report: &JsonlReporter) {
    let ident = format!(
        "app-e2e-log-{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let marker = format!("e2e-log-marker-{ident}");
    let user = "e2e-user";

    // 前置：workspace（dev 卷载体）
    let (ws_s, ws_b) = post_json(
        env,
        "/api/v1/userapp/workspace",
        json!({"app_id": ident, "user_id": user}),
    )
    .await;
    let ws_ok = ws_s.is_success() && http_ok(&ws_b);
    report.assert_hard(
        "log 场景前置：create-workspace",
        ws_ok,
        format!("HTTP {ws_s}, body 截断: {}", trunc(&ws_b, 120)),
    );
    if !ws_ok {
        cleanup_builder(&ident);
        return;
    }

    // files-update 写 .logs 探针（get-logs 读 {ws}/.logs 下 mtime 最新文件尾行；
    // 镜像族须 X-App-Id 定位开发容器，post_json 不带 header 故直构请求）
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/files-update", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &ident)
        .json(&json!({
            "app_id": ident, "user_id": user,
            "files": [{"operation": "create", "name": ".logs/probe.log", "contents": marker}]
        }))
        .send()
        .await
        .expect("files-update post");
    let fu_s = resp.status();
    let fu_b: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "files-update 写 .logs/probe.log 探针",
        fu_s.is_success() && fu_b["success"].as_bool() == Some(true),
        format!("HTTP {fu_s}, body 截断: {}", trunc(&fu_b, 120)),
    );

    // get-logs 尾读（镜像族：X-App-Id 定位容器 + snake query）
    let resp = env
        .http
        .get(format!(
            "{}/api/v1/userapp/get-logs?app_id={ident}&user_id={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &ident)
        .send()
        .await
        .expect("get-logs");
    let gl_s = resp.status();
    let gl_b: Value = resp.json().await.unwrap_or(Value::Null);
    let marker_seen = gl_s.is_success()
        && gl_b["success"].as_bool() == Some(true)
        && gl_b["total_lines"].as_u64().unwrap_or(0) >= 1
        && gl_b["logs"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|r| r["content"].as_str().is_some_and(|c| c.contains(&marker)))
        });
    report.assert_hard(
        "get-logs 尾读回显探针行",
        marker_seen,
        format!("HTTP {gl_s}, body 截断: {}", trunc(&gl_b, 180)),
    );

    // query：运行时对账（实时查集群 Deployment 集）——workspace-only app 无
    // prod 运行时，过滤 app_ids 应得空列表（语义锚：query 是运行态而非元数据面）
    let (q_s, q_b) = post_json(
        env,
        "/api/v1/userapp/query",
        json!({"user_id": user, "page": 1, "page_size": 50, "filters": {"app_ids": [ident]}}),
    )
    .await;
    let listed = q_s.is_success()
        && http_ok(&q_b)
        && q_b["data"]["items"]
            .as_array()
            .is_some_and(|items| !items.iter().any(|i| i["app_id"] == ident));
    report.assert_hard(
        "query 运行时对账：未部署 app 不在运行时列表",
        listed,
        format!("HTTP {q_s}, body 截断: {}", trunc(&q_b, 150)),
    );

    // runtime：运行时列表（无 prod 容器时为空数组——断言接口形态非内容）
    let (rt_s, rt_b) = get_json(env, &format!("/api/v1/userapp/runtime?user_id={user}")).await;
    report.assert_hard(
        "runtime 运行时列表（200 + 0000）",
        rt_s.is_success() && http_ok(&rt_b),
        format!("HTTP {rt_s}, body 截断: {}", trunc(&rt_b, 120)),
    );

    // 回收 dev 环境
    drop(
        post_json(
            env,
            &format!("/api/v1/userapp/{ident}/dev/storage/destroy"),
            json!({"user_id": user, "confirm": ident}),
        )
        .await,
    );
    cleanup_builder(&ident);
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
    test_dev_logs_and_listing(&env, &report).await;
    test_query_pagination_validation(&env, &report).await;
    test_update_stop_restart(&env, &report).await;
    test_storage_guards(&env, &report).await;
    test_recycle_policy(&env, &report).await;
    test_observation_error_shapes(&env, &report).await;
    test_terminal_proxy_redirects(&env, &report).await;
    test_ensure_workspace_idempotent(&env, &report).await;

    let path = report.path.display().to_string();
    assert!(report.finish(), "场景失败：断言明细见 {path}");
}

/// A1 query 排序/分页校验（400 三态 + 合法形态）。
async fn test_query_pagination_validation(env: &Env, report: &JsonlReporter) {
    let user = "e2e-user";
    // 400：page < 1
    let (s, _) = post_json(
        env,
        "/api/v1/userapp/query",
        json!({"user_id": user, "page": 0}),
    )
    .await;
    report.assert_hard("query page=0 → 400", s.as_u16() == 400, format!("HTTP {s}"));
    // 400：page_size 超界（>100）
    let (s, _) = post_json(
        env,
        "/api/v1/userapp/query",
        json!({"user_id": user, "page_size": 101}),
    )
    .await;
    report.assert_hard(
        "query page_size=101 → 400（1..=100）",
        s.as_u16() == 400,
        format!("HTTP {s}"),
    );
    // 400：非法 sort_by
    let (s, _) = post_json(
        env,
        "/api/v1/userapp/query",
        json!({"user_id": user, "sort_by": "bogus_field"}),
    )
    .await;
    report.assert_hard(
        "query sort_by=bogus → 400（仅 app_id/name/created_at）",
        s.as_u16() == 400,
        format!("HTTP {s}"),
    );
    // 合法：sort_by=app_id + 分页结构
    let (s, b) = post_json(
        env,
        "/api/v1/userapp/query",
        json!({"user_id": user, "sort_by": "app_id", "page": 1, "page_size": 5}),
    )
    .await;
    let ok = s.is_success()
        && http_ok(&b)
        && b["data"]["pagination"]["page"].as_u64() == Some(1)
        && b["data"]["pagination"]["page_size"].as_u64() == Some(5)
        && b["data"]["items"].as_array().is_some();
    report.assert_hard(
        "query 合法形态（sort_by=app_id + 分页结构）",
        ok,
        format!("HTTP {s}, body 截断: {}", trunc(&b, 120)),
    );
}

/// A2 update/stop/restart 语义闭环。
async fn test_update_stop_restart(env: &Env, report: &JsonlReporter) {
    let suffix = format!(
        "{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let user = "e2e-user";

    // 404：update / stop 不存在 app
    let (s, _) = post_json(
        env,
        &format!("/api/v1/userapp/app-e2e-noup-{suffix}/update"),
        json!({"user_id": user}),
    )
    .await;
    report.assert_hard(
        "update 不存在 app → 404",
        s.as_u16() == 404,
        format!("HTTP {s}"),
    );
    let (s, _) = post_json(
        env,
        &format!("/api/v1/userapp/app-e2e-nostop-{suffix}/stop?user_id={user}"),
        json!({}),
    )
    .await;
    report.assert_hard(
        "stop 不存在 app → 404",
        s.as_u16() == 404,
        format!("HTTP {s}"),
    );
    // 400：stop 缺 user_id query
    let (s, _) = post_json(env, "/api/v1/userapp/app-e2e-any/stop", json!({})).await;
    report.assert_hard(
        "stop 缺 user_id query → 400",
        s.as_u16() == 400,
        format!("HTTP {s}"),
    );

    // 闭环：空容器 → update{name} → stop → 唤醒
    let app_id = format!("app-e2e-usr-{suffix}");
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app_id}/start"),
        json!({"user_id": user}),
    )
    .await;
    let created = s.is_success() && http_ok(&b);
    report.assert_hard(
        "update/stop 前置：start 空容器",
        created,
        format!("HTTP {s}, body 截断: {}", trunc(&b, 120)),
    );
    if !created {
        return;
    }
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app_id}/update"),
        json!({"user_id": user, "name": "e2e-renamed"}),
    )
    .await;
    report.assert_hard(
        "update{name} → 200（live 回退其余字段）",
        s.is_success() && http_ok(&b),
        format!("HTTP {s}, body 截断: {}", trunc(&b, 120)),
    );

    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app_id}/stop?user_id={user}"),
        json!({}),
    )
    .await;
    let stopped = s.is_success()
        && http_ok(&b)
        && b["data"]["status"].as_str() == Some("stopped")
        && b["data"]["replicas"].as_u64() == Some(0);
    report.assert_hard(
        "stop → stopped / replicas=0",
        stopped,
        format!("HTTP {s}, body 截断: {}", trunc(&b, 150)),
    );

    // 唤醒：start（无 url 传统启动=唤醒通道）；K8s 异步就绪轮询到 running
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app_id}/start"),
        json!({"user_id": user}),
    )
    .await;
    let accepted = s.is_success() && http_ok(&b);
    let mut st = b["data"]["status"].as_str().unwrap_or("").to_string();
    if accepted {
        let t0 = Instant::now();
        while st != "running" && t0.elapsed() < Duration::from_secs(120) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let resp = env
                .http
                .get(format!(
                    "{}/api/v1/userapp/{app_id}?user_id={user}",
                    env.rcoder
                ))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
            if let Ok(r) = resp
                && let Ok(v) = r.json::<Value>().await
            {
                st = v["data"]["status"].as_str().unwrap_or("").to_string();
            }
        }
    }
    report.assert_hard(
        "stop 后 start 唤醒 → running（120s 轮询兼容 K8s 异步）",
        accepted && st == "running",
        format!("HTTP {s}, status={st}"),
    );

    drop(
        post_json(
            env,
            &format!("/api/v1/userapp/{app_id}/prod/delete"),
            json!({"user_id": user, "purge": true}),
        )
        .await,
    );
}

/// A3 storage 守卫（GET 幂等 + clear/destroy 前置）。
async fn test_storage_guards(env: &Env, report: &JsonlReporter) {
    let user = "e2e-user";
    // GET 随机 app：200 + exists=false（不校验 app 存在性）
    let (s, b) = get_json(
        env,
        &format!("/api/v1/userapp/app-e2e-ghost-{user}/prod/storage?user_id={user}"),
    )
    .await;
    report.assert_hard(
        "storage GET 不存在 app → 200 + exists=false",
        s.is_success() && http_ok(&b) && b["data"]["exists"] == false,
        format!("HTTP {s}, body 截断: {}", trunc(&b, 120)),
    );
    // clear 未 delete → 409（前置：app 有计算资源——Docker 模式下 ghost app 的
    // deployment 查询返回 None → 守卫通过 → 幂等成功；须真实容器验证守卫分支）
    let guard_app = format!(
        "app-e2e-clr-{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let (cs, _) = post_json(
        env,
        &format!("/api/v1/userapp/{guard_app}/start"),
        json!({"user_id": user}),
    )
    .await;
    if cs.is_success() {
        let (s, b) = post_json(
            env,
            &format!("/api/v1/userapp/{guard_app}/prod/storage/clear"),
            json!({"user_id": user}),
        )
        .await;
        report.assert_hard(
            "storage/clear 对未 delete 的 app → 409",
            s.as_u16() == 409,
            format!("HTTP {s}, body 截断: {}", trunc(&b, 100)),
        );
        drop(
            post_json(
                env,
                &format!("/api/v1/userapp/{guard_app}/prod/delete"),
                json!({"user_id": user, "purge": true}),
            )
            .await,
        );
    } else {
        report.assert_hard(
            "storage/clear 前置：start 空容器",
            false,
            format!("HTTP {cs}"),
        );
    }
    // destroy confirm 不匹配 → 400
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/app-e2e-ghost-{user}/prod/storage/destroy"),
        json!({"user_id": user, "confirm": "wrong-confirm"}),
    )
    .await;
    report.assert_hard(
        "storage/destroy confirm≠app_id → 400",
        s.as_u16() == 400,
        format!("HTTP {s}, body 截断: {}", trunc(&b, 100)),
    );
}

/// A4 recycle-policy 守卫与成功。
async fn test_recycle_policy(env: &Env, report: &JsonlReporter) {
    let user = "e2e-user";
    let suffix = format!(
        "{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    // 400：stage=dev
    let (s, _) = post_json(
        env,
        &format!("/api/v1/userapp/app-e2e-rc-{suffix}/dev/recycle-policy"),
        json!({"user_id": user, "recycle_enabled": true}),
    )
    .await;
    report.assert_hard(
        "recycle-policy stage=dev → 400（仅 prod）",
        s.as_u16() == 400,
        format!("HTTP {s}"),
    );
    // 400：三可选字段全缺
    let (s, _) = post_json(
        env,
        &format!("/api/v1/userapp/app-e2e-rc-{suffix}/prod/recycle-policy"),
        json!({"user_id": user}),
    )
    .await;
    report.assert_hard(
        "recycle-policy 三字段全缺 → 400",
        s.as_u16() == 400,
        format!("HTTP {s}"),
    );
    // 400：非法 user_id
    let (s, _) = post_json(
        env,
        &format!("/api/v1/userapp/app-e2e-rc-{suffix}/prod/recycle-policy"),
        json!({"user_id": "bad user!", "recycle_enabled": true}),
    )
    .await;
    report.assert_hard(
        "recycle-policy 非法 user_id → 400",
        s.as_u16() == 400,
        format!("HTTP {s}"),
    );

    // 成功：空容器上设置
    let app_id = format!("app-e2e-rc-{suffix}");
    let (cs, cb) = post_json(
        env,
        &format!("/api/v1/userapp/{app_id}/start"),
        json!({"user_id": user}),
    )
    .await;
    if !(cs.is_success() && http_ok(&cb)) {
        report.assert_hard(
            "recycle-policy 前置：start 空容器",
            false,
            format!("HTTP {cs}"),
        );
        return;
    }
    let (s, b) = post_json(
        env,
        &format!("/api/v1/userapp/{app_id}/prod/recycle-policy"),
        json!({"user_id": user, "recycle_enabled": false, "idle_timeout_seconds": 3600}),
    )
    .await;
    report.assert_hard(
        "recycle-policy prod 成功（200 + runtime 回显）",
        s.is_success() && http_ok(&b) && b["data"]["app_id"] == app_id,
        format!("HTTP {s}, body 截断: {}", trunc(&b, 150)),
    );
    drop(
        post_json(
            env,
            &format!("/api/v1/userapp/{app_id}/prod/delete"),
            json!({"user_id": user, "purge": true}),
        )
        .await,
    );
}

/// A5 观测接口错误形态（404/400）。
async fn test_observation_error_shapes(env: &Env, report: &JsonlReporter) {
    let user = "e2e-user";
    let ghost = "app-e2e-ghost-obs";
    // health 不存在 app → 404
    let (s, _) = get_json(
        env,
        &format!("/api/v1/userapp/{ghost}/prod/health?user_id={user}"),
    )
    .await;
    report.assert_hard(
        "health 不存在 app → 404",
        s.as_u16() == 404,
        format!("HTTP {s}"),
    );
    // logs sources/query 不存在 app → 404
    let (s, _) = post_json(
        env,
        &format!("/api/v1/userapp/{ghost}/prod/logs/sources/query?user_id={user}"),
        json!({}),
    )
    .await;
    report.assert_hard(
        "logs/sources/query 不存在 app → 404",
        s.as_u16() == 404,
        format!("HTTP {s}"),
    );
    // logs/query 不存在 app → 404
    let (s, _) = post_json(
        env,
        &format!("/api/v1/userapp/{ghost}/prod/logs/query?user_id={user}"),
        json!({"tail": 10}),
    )
    .await;
    report.assert_hard(
        "logs/query 不存在 app → 404",
        s.as_u16() == 404,
        format!("HTTP {s}"),
    );
    // 非法 stage → 400
    let (s, _) = get_json(
        env,
        &format!("/api/v1/userapp/{ghost}/staging/health?user_id={user}"),
    )
    .await;
    report.assert_hard(
        "health 非法 stage → 400",
        s.as_u16() == 400,
        format!("HTTP {s}"),
    );
}

/// 禁跟随重定向的 client（307 断言用——共享 client 自动 follow 会打到
/// Pingora 宿主端口而 compose 映射是 8089≠8088）。
fn no_redirect_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build no-redirect client")
    })
}

/// A6 终端代理主端口 307 文档重定向（零依赖，恒可断言）。
async fn test_terminal_proxy_redirects(env: &Env, report: &JsonlReporter) {
    let user = "e2e-user";
    // 三个工具族变体：307 + Location 是绝对 URL 指向 Pingora 入口
    // （{*path} 需非空段——尾斜杠空段不匹配会 404，root 用无尾变体/带子路径）
    for (tool, path) in [
        ("ttyd", "ttyd/dev/{user}/{app}"),
        ("dbx", "dbx/prod/{user}/{app}/ws"),
        ("vnc", "vnc/dev/{user}/{app}/x"),
    ] {
        let app = format!("app-e2e-{tool}");
        let url = format!(
            "/api/v1/userapp/proxy/{}",
            path.replace("{user}", user).replace("{app}", &app)
        );
        let resp = no_redirect_client()
            .get(format!("{}{url}", env.rcoder))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .expect("redirect get");
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let ok = status == 307
            && location.contains("://")
            && location.contains("/api/v1/userapp/proxy/");
        report.assert_hard(
            &format!("终端代理[{tool}] 主端口 → 307 + Location 重定向"),
            ok,
            format!("HTTP {status}, Location: {location}"),
        );
    }
    // 文档速查表
    let resp = env
        .http
        .get(format!("{}/userapp/routes", env.rcoder))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("routes get");
    report.assert_hard(
        "/userapp/routes 速查表 → 200",
        resp.status().is_success(),
        format!("HTTP {}", resp.status()),
    );
}

/// A7 ensure-workspace 幂等。
async fn test_ensure_workspace_idempotent(env: &Env, report: &JsonlReporter) {
    let ident = format!(
        "app-e2e-ew-{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let user = "e2e-user";
    let call = || async {
        let resp = env
            .http
            .post(format!("{}/api/v1/userapp/ensure-workspace", env.rcoder))
            .timeout(Duration::from_secs(120))
            .header("X-App-Id", &ident)
            .json(&json!({"app_id": ident, "user_id": user}))
            .send()
            .await
            .expect("ensure post");
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    };
    let (s1, b1) = call().await;
    let first_ok = s1.is_success()
        && http_ok(&b1)
        && b1["data"]["workspace"]
            .as_str()
            .is_some_and(|w| !w.is_empty());
    report.assert_hard(
        "ensure-workspace 首调（懒建容器）",
        first_ok,
        format!("HTTP {s1}, body 截断: {}", trunc(&b1, 120)),
    );
    if !first_ok {
        cleanup_builder(&ident);
        return;
    }
    let (s2, b2) = call().await;
    let idem =
        s2.is_success() && http_ok(&b2) && b1["data"]["workspace"] == b2["data"]["workspace"];
    report.assert_hard(
        "ensure-workspace 幂等（两次 workspace 路径一致）",
        idem,
        format!(
            "1st={:?} 2nd={:?}",
            b1["data"]["workspace"].as_str(),
            b2["data"]["workspace"].as_str()
        ),
    );
    cleanup_builder(&ident);
}
