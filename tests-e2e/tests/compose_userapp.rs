//! compose 环境 UserApp 面回归（tasks/query / publish 编排终态 / 创建约束）。
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

/// 显式清理 publish 触发的 builder 容器（rcoder-app-builder-<app_id>；
/// TestUserGuard 只清 agent-runner 前缀，builder 需场景自理）。
fn cleanup_builder(app_id: &str) {
    let name = format!("rcoder-app-builder-{app_id}");
    std::process::Command::new("docker")
        .args(["rm", "-f", &name])
        .output()
        .ok();
}

/// tasks/query 接口（compose = 内存任务表）：分页结构 + 过滤。
async fn test_tasks_query(env: &Env, report: &JsonlReporter) {
    let (status, body) = post_json(env, "/api/v1/apps/publish/tasks/query", json!({})).await;
    let ok = status.is_success() && http_ok(&body);
    let structure_ok =
        ok && body["data"]["items"].is_array() && body["data"]["pagination"]["total"].is_i64();
    report.assert_hard(
        "tasks/query 空查询返回 200 + 分页结构（items + pagination）",
        structure_ok,
        format!(
            "HTTP {status}, total={:?}",
            body["data"]["pagination"]["total"].as_i64()
        ),
    );

    let (status2, _) = post_json(
        env,
        "/api/v1/apps/publish/tasks/query",
        json!({"filters": {"activeOnly": true}}),
    )
    .await;
    report.assert_hard(
        "activeOnly 过滤查询返回 200",
        status2.is_success(),
        format!("HTTP {status2}"),
    );
}

/// publish/build 标识校验：非法 app_id 快速失败（不挂起——activate 死锁修复的行为面）。
async fn test_publish_identifiers(env: &Env, report: &JsonlReporter) {
    for kind in ["publish", "build"] {
        let t0 = Instant::now();
        let (status, _) = post_json(
            env,
            &format!("/api/v1/apps/BAD_ID/{kind}"),
            json!({"projectId": "also-bad"}),
        )
        .await;
        let elapsed = t0.elapsed();
        report.assert_hard(
            &format!("{kind} 非法 app_id 返回 4xx 且不挂起"),
            status.as_u16() >= 400 && status.as_u16() < 500 && elapsed < Duration::from_secs(5),
            format!("HTTP {status}, {:.1}s", elapsed.as_secs_f64()),
        );
    }
    // projectId != appId：UserAppBuilder 一 app 一 workspace 契约
    let (status, body) = post_json(
        env,
        "/api/v1/apps/app-e2e-mismatch-x1/publish",
        json!({"projectId": "proj-different"}),
    )
    .await;
    report.assert_hard(
        "projectId != appId 被拒（400 契约校验）",
        status.as_u16() == 400
            && body["message"]
                .as_str()
                .is_some_and(|m| m.contains("must equal")),
        format!(
            "HTTP {status}, {:?}",
            body["message"].as_str().unwrap_or("")
        ),
    );
}

/// publish 合法请求 → 有限时间内到达终态（不挂死）+ tasks/query 可过滤。
async fn test_publish_reaches_terminal(env: &Env, report: &JsonlReporter) {
    // ident 唯一化：run_tag(秒级)+pid——不同测试进程同秒启动也不撞名
    // （publish 对同 app 的活跃任务重复受理会 409）
    let ident = format!(
        "app-e2e-rs-{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let guard_app = ident.clone();

    let (status, body) = post_json(
        env,
        &format!("/api/v1/apps/{ident}/publish"),
        json!({"projectId": ident}),
    )
    .await;
    let ok = status.is_success() && http_ok(&body);
    report.assert_hard(
        "publish 受理（200 任务创建）",
        ok,
        format!("HTTP {status}, body 截断: {}", trunc(&body, 120)),
    );
    if !ok {
        cleanup_builder(&guard_app);
        return;
    }
    let task_id = body["data"]["taskId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    // 轮询到终态（failed/cancelled/completed）；agent workspace 不存在 → 业务性 failed
    let t0 = Instant::now();
    let mut terminal: Option<String> = None;
    while t0.elapsed() < Duration::from_secs(180) {
        let resp = env
            .http
            .get(format!(
                "{}/api/v1/apps/publish/tasks/{task_id}",
                env.rcoder
            ))
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        if let Ok(r) = resp
            && r.status().is_success()
            && let Ok(b) = r.json::<Value>().await
        {
            let status = b["data"]["task"]["status"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase();
            if matches!(status.as_str(), "failed" | "cancelled" | "completed") {
                terminal = Some(status);
                report.chat_request(rcoder_e2e::common::report::ChatTrace {
                    phase: "terminal_poll",
                    url: &env.rcoder,
                    ok: true,
                    request_sanitized: json!({"taskId": task_id}),
                    response: Some(&b["data"]["task"]),
                    error: None,
                    elapsed_ms: t0.elapsed().as_millis(),
                });
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    cleanup_builder(&guard_app);
    report.assert_hard(
        "publish 任务 180s 内到达终态（不挂死）",
        terminal.is_some(),
        match &terminal {
            Some(s) => format!("status={s}, {:.0}s", t0.elapsed().as_secs_f64()),
            None => "180s 未到终态（疑似挂死）".to_owned(),
        },
    );

    // tasks/query 按 appIds 过滤应能看到该任务
    let (qs, qb) = post_json(
        env,
        "/api/v1/apps/publish/tasks/query",
        json!({"filters": {"appIds": [ident]}}),
    )
    .await;
    let hit = qs.is_success()
        && qb["data"]["items"]
            .as_array()
            .is_some_and(|a| !a.is_empty());
    report.assert_hard(
        "tasks/query 可按 appIds 过滤到该任务",
        hit,
        format!(
            "HTTP {qs}, 命中 {}",
            qb["data"]["items"].as_array().map_or(0, Vec::len)
        ),
    );
}

/// 直接 create_app 被发布流水线约束拦截（UserApp 语义：app 容器只能由发布编排创建）。
async fn test_app_create_requires_release_lock(env: &Env, report: &JsonlReporter) {
    let app_id = format!(
        "app-e2e-nolock-{}{}",
        &env.run_tag.replace('_', "")[..10],
        std::process::id() % 1000
    );
    let payload = json!({
        "appId": app_id,
        "name": "e2e-nolock-test",
        "image": "alpine:3.19",
        "command": ["sleep", "3600"],
    });
    let (status, body) = post_json(env, "/api/v1/apps", payload).await;
    let rejected = (status.as_u16() == 400 || status.as_u16() == 409 || status.as_u16() == 500)
        && body["message"]
            .as_str()
            .is_some_and(|m| m.contains("release lock"));
    report.assert_hard(
        "无 release lock 创建被拒（ERR_INVALID_STATE）",
        rejected,
        format!("HTTP {status}, {}", trunc(&body, 150)),
    );
    let resp = env
        .http
        .get(format!("{}/api/v1/apps/{app_id}", env.rcoder))
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    let get_status = resp.map(|r| r.status().as_u16()).unwrap_or(0);
    report.assert_hard(
        "失败后无残留 app",
        get_status == 404,
        format!("GET HTTP {get_status}"),
    );
}

fn trunc(v: &Value, n: usize) -> String {
    let s = v.to_string();
    s.chars().take(n).collect()
}

#[tokio::test]
async fn userapp_compose_regression() {
    let scenario = "userapp_compose";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    test_tasks_query(&env, &report).await;
    test_publish_identifiers(&env, &report).await;
    test_publish_reaches_terminal(&env, &report).await;
    test_app_create_requires_release_lock(&env, &report).await;

    let path = report.path.display().to_string();
    assert!(report.finish(), "场景失败：断言明细见 {path}");
}
