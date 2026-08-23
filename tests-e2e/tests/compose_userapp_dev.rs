//! compose 环境 UserApp **开发域**场景：per-app 开发容器上的文件转发/拦截分流、
//! PG 凭据对齐、`/computer/chat + service_type=userapp` 开发对话 + SSE 消息流。
//!
//! 运行: `cargo test -p rcoder-e2e --test compose_userapp_dev -- --test-threads=1`
//! （与 compose_sse 同门控：RCODER_URL /health 可达；LLM 场景另需模型配置完整）
//!
//! 覆盖点（对应 rcoder userapp_forward / computer_chat_handler userApp 分支）：
//! - create-workspace 起手（ensure 开发容器 + 建目录 + owner 注册）
//! - X-App-Id 直连转发 + X-Service-Type 拦截分流（两路落同一 workspace）
//! - `/api/userapp/db/dev/align-credentials`（scram 验证 → trust 重置 → 复验）
//! - userApp 开发对话全轮：session 创建（project_id=app_id 回显）+ SSE 事件流
//!   （/computer/progress/{sid} 经 session→project 映射路由到开发容器）

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// 套件级串行锁：单节点（mac Docker Desktop / K8s 单节点）资源天花板下，多场景并行
/// 建 builder 容器会拖慢后发容器的 agent_runner 启动（60000 连接超退避窗）。
/// 代码级固化串行，免去 --test-threads=1 依赖。
static SCENARIO_GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn scenario_gate() -> tokio::sync::MutexGuard<'static, ()> {
    SCENARIO_GATE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

use rcoder_e2e::common::report::JsonlReporter;
use rcoder_e2e::common::scenario::{CollectSpec, assert_hard_all, collect_reported, count_event};
use rcoder_e2e::common::sse;
use rcoder_e2e::common::{Backend, Env, chat_reported};
use serde_json::{Value, json};

fn http_ok(body: &Value) -> bool {
    body["code"].as_str() == Some("0000")
}

async fn post_json(env: &Env, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let resp = env
        .http
        .post(format!("{}{path}", env.rcoder))
        .timeout(Duration::from_secs(90))
        .json(&body)
        .send()
        .await
        .expect("http post");
    let status = resp.status();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// 场景内唯一 app_id（run_tag+pid 防跨进程撞名；≤63 字符约束内）。
fn scoped_app(env: &Env, tag: &str) -> String {
    format!("e2e-ud-{}-{}", &env.run_tag.replace('_', "")[..10], tag,)
        .chars()
        .take(48)
        .collect()
}

/// 显式清理开发容器（Docker: docker rm；K8s 模式由 rcoder 闲置回收兜底，
/// 测试内不等待——场景各自创建唯一 app_id 不复用）。
fn cleanup_builder(app_id: &str) {
    let name = format!("rcoder-app-builder-{app_id}");
    std::process::Command::new("docker")
        .args(["rm", "-f", &name])
        .output()
        .ok();
}

/// create-workspace（幂等起手；断言 200 + 容器信息回显）。
async fn create_workspace(env: &Env, report: &JsonlReporter, app_id: &str, user: &str) -> bool {
    let (status, body) = post_json(
        env,
        "/api/userapp/workspace",
        json!({"appId": app_id, "userId": user}),
    )
    .await;
    let ok = status.is_success()
        && http_ok(&body)
        && body["data"]["containerName"]
            .as_str()
            .is_some_and(|n| n.contains(app_id));
    report.assert_hard(
        "create-workspace（ensure 开发容器+建目录+owner 注册）",
        ok,
        format!("HTTP {status}, body 截断: {}", trunc(&body, 120)),
    );
    ok
}

fn trunc(v: &Value, n: usize) -> String {
    let s = v.to_string();
    s.chars().take(n).collect()
}

// ============================================================
// 场景 1：文件两路入口（X-App-Id 直连转发 + X-Service-Type 拦截分流）
//          落同一 workspace；缺 X-App-Id 拒 400（无 LLM 依赖）
// ============================================================
#[tokio::test]
async fn userapp_dev_files_two_entry_points() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_files";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "f1");
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 入口 A：userApp 新接口直连（X-App-Id 定位开发容器——post_json 不带 header，直接构造）
    let resp_a = env
        .http
        .post(format!("{}/api/userapp/generate-file", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .json(&json!({"appId": app, "userId": user, "fileName": "direct.txt", "content": "via direct"}))
        .send()
        .await
        .expect("direct post");
    let sa = resp_a.status();
    let ba: Value = resp_a.json().await.unwrap_or(Value::Null);
    let ok_a = sa.is_success() && ba["success"].as_bool() == Some(true);
    report.assert_hard(
        "入口 A：/api/userapp/generate-file（X-App-Id 转发）",
        ok_a,
        format!("HTTP {sa}, {}", trunc(&ba, 100)),
    );

    // 入口 B：TS 老路径 + 双 header（拦截层短路转发同路径）
    let resp = env
        .http
        .post(format!("{}/api/computer/generate-file", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-Service-Type", "userapp")
        .header("X-App-Id", &app)
        .json(&json!({"userId": user, "cId": app, "fileName": "proxy.txt", "content": "via proxy"}))
        .send()
        .await
        .expect("intercepted post");
    let status_b = resp.status();
    let body_b: Value = resp.json().await.unwrap_or(Value::Null);
    let ok_b = status_b.is_success() && body_b["success"].as_bool() == Some(true);
    report.assert_hard(
        "入口 B：/api/computer/generate-file + X-Service-Type/X-App-Id（拦截分流）",
        ok_b,
        format!("HTTP {status_b}, {}", trunc(&body_b, 100)),
    );

    // 两路落同一 workspace：get-file-list 应同时看到两个文件
    let resp_l = env
        .http
        .get(format!(
            "{}/api/userapp/get-file-list?appId={app}&userId={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .send()
        .await
        .expect("list get");
    let sl = resp_l.status();
    let bl: Value = resp_l.json().await.unwrap_or(Value::Null);
    let files = bl["files"].as_array().cloned().unwrap_or_default();
    let names: Vec<String> = files
        .iter()
        .filter_map(|f| f["name"].as_str().map(str::to_owned))
        .collect();
    let both = names.iter().any(|n| n == "direct.txt") && names.iter().any(|n| n == "proxy.txt");
    report.assert_hard(
        "两路入口落同一 workspace（list 同时含 direct.txt 与 proxy.txt）",
        both,
        format!("HTTP {sl}, files: {names:?}"),
    );

    // 缺 X-App-Id 的 userApp 转发 → 400（明确提示）
    let resp = env
        .http
        .post(format!("{}/api/userapp/generate-file", env.rcoder))
        .timeout(Duration::from_secs(15))
        .json(&json!({"appId": app, "userId": user, "fileName": "x.txt", "content": "x"}))
        .send()
        .await
        .expect("missing header post");
    report.assert_hard(
        "缺 X-App-Id → 400",
        resp.status().as_u16() == 400,
        format!("HTTP {}", resp.status()),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// 场景 2：PG 凭据对齐 dev（全新容器 PG initdb 有就绪窗口，轮询收敛）
//          首调 aligned=true；同密码复调 reset_performed=false（无 LLM 依赖）
// ============================================================
#[tokio::test]
async fn userapp_dev_pg_align_idempotent() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_pg_align";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "db1");
    let user = "e2e-ud-user";
    let pw = "e2e-align-pw-01";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 全新容器的 PG initdb 需要时间（镜像全套启动）；连接类失败重试收敛
    let mut first: Option<Value> = None;
    let t0 = Instant::now();
    let deadline = Duration::from_secs(120);
    while t0.elapsed() < deadline {
        let (s, b) = post_json(
            &env,
            "/api/userapp/db/dev/align-credentials",
            json!({"app_id": app, "username": "dev", "password": pw}),
        )
        .await;
        if s.is_success() && http_ok(&b) {
            first = Some(b["data"].clone());
            break;
        }
        report.diagnostic(
            "pg align retry（PG initdb 就绪窗口）",
            &s.to_string(),
            &trunc(&b, 120),
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let ok_first = first
        .as_ref()
        .is_some_and(|d| d["aligned"].as_bool() == Some(true));
    report.assert_hard(
        "首次对齐成功（scram 验证/trust 重置/复验）",
        ok_first,
        format!("data: {:?}", first.as_ref().map(|d| trunc(d, 80))),
    );
    if !ok_first {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 同密码复调：一致 → reset_performed=false（幂等语义）
    let (_, b2) = post_json(
        &env,
        "/api/userapp/db/dev/align-credentials",
        json!({"app_id": app, "username": "dev", "password": pw}),
    )
    .await;
    let ok_second = http_ok(&b2) && b2["data"]["reset_performed"].as_bool() == Some(false);
    report.assert_hard(
        "同密码复调 reset_performed=false（幂等）",
        ok_second,
        trunc(&b2, 120),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// 场景 3：userApp 开发对话全轮 + SSE（双后端）
//          chat service_type=userapp → 开发容器 agent_runner；
//          SSE 经 /computer/progress/{sid}（session→project 映射路由）
// ============================================================
async fn scenario_userapp_chat_full_turn(backend: Backend) {
    let scenario = "userapp_dev_chat_full_turn";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let app = scoped_app(&env, &format!("c-{}", backend.as_str()));
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // chat 请求：computer 域基础 payload + userApp 开发对话标记
    let mut req = env.base_payload(
        backend,
        "从1数到6，每行一个数字",
        &format!("{}-udc", env.run_tag),
        user,
    );
    req.service_type = Some(shared_types::ChatServiceScope::Userapp);
    req.project_id = Some(app.clone());

    let Ok(data) = chat_reported(&env, &report, "turn1", &env.rcoder, &req).await else {
        report.assert_hard("chat 成功", false, "chat 失败（见 chat_request 行）".into());
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };
    let sid = data.session_id.clone();
    report.assert_hard("session_id 非空", !sid.is_empty(), sid.clone());
    // userApp 特有：project_id 回显 = app_id（路由到该 app 开发容器的锚点）
    report.assert_hard(
        "project_id 回显 = app_id",
        data.project_id == app,
        format!("回显 {:?}，期望 {app:?}", data.project_id),
    );

    tokio::time::sleep(Duration::from_millis(800)).await;
    let (events, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "collect_turn1",
            entry: &env.rcoder,
            sid: &sid,
            duration_s: 30.0,
            last_event_id: None,
            idle_stop: true,
        },
    )
    .await;
    let ids = sse::ids_of(&events);
    let types = sse::type_counts(&events);
    let text = sse::chunks_text(&events);

    report.assert_hard(
        "含 prompt_start",
        count_event(&events, "prompt_start") >= 1,
        format!("事件分布 {types}"),
    );
    report.assert_hard(
        "含 end_turn（完整轮）",
        count_event(&events, "end_turn") >= 1,
        "完整轮执行".into(),
    );
    report.assert_hard(
        "含流式 chunk",
        count_event(&events, "agent_message_chunk") >= 1,
        format!("{} 个", count_event(&events, "agent_message_chunk")),
    );
    report.assert_hard(
        "id 单调无重复",
        sse::monotonic_unique(&ids),
        format!("{} 个 seq", ids.len()),
    );
    report.assert_hard(
        "回答含数字内容",
        text.chars().any(|ch| ch.is_ascii_digit()),
        format!("回答头部 {:?}", text.chars().take(30).collect::<String>()),
    );
    report.diagnostic("回答文本", &text, "agent_message_chunk 拼接全文");

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

#[tokio::test]
async fn userapp_dev_chat_full_turn_openai() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    scenario_userapp_chat_full_turn(Backend::Openai).await;
}

#[tokio::test]
async fn userapp_dev_chat_full_turn_anthropic() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    scenario_userapp_chat_full_turn(Backend::Anthropic).await;
}

// ============================================================
// 场景 4：userApp 开发对话两轮 seq 隔离（同 session 第二轮不含第一轮内容）
// ============================================================
async fn scenario_userapp_two_turn_isolation(backend: Backend) {
    let scenario = "userapp_dev_two_turn";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let app = scoped_app(&env, &format!("t-{}", backend.as_str()));
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    let mk_req = |prompt: &str, tag: &str| {
        let mut r = env.base_payload(backend, prompt, &format!("{}-{tag}", env.run_tag), user);
        r.service_type = Some(shared_types::ChatServiceScope::Userapp);
        r.project_id = Some(app.clone());
        r
    };

    let Ok(d1) = chat_reported(
        &env,
        &report,
        "turn1",
        &env.rcoder,
        &mk_req("从1数到4，每行一个数字", "udt1"),
    )
    .await
    else {
        report.assert_hard(
            "turn1 chat 成功",
            false,
            "chat 失败（见 chat_request 行）".into(),
        );
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };
    let sid = d1.session_id.clone();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let (evs1, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "collect_turn1",
            entry: &env.rcoder,
            sid: &sid,
            duration_s: 30.0,
            last_event_id: None,
            idle_stop: true,
        },
    )
    .await;
    let text1 = sse::chunks_text(&evs1);

    // 第二轮（显式带 session_id 续话）
    let mut req2 = mk_req("从5数到8，每行一个数字", "udt2");
    req2.session_id = Some(sid.clone());
    let Ok(d2) = chat_reported(&env, &report, "turn2", &env.rcoder, &req2).await else {
        report.assert_hard(
            "turn2 chat 成功",
            false,
            "chat 失败（见 chat_request 行）".into(),
        );
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };
    report.assert_hard(
        "turn2 复用同一 session",
        d2.session_id == sid,
        format!("turn1 {sid:?} vs turn2 {:?}", d2.session_id),
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    let (evs2, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "collect_turn2",
            entry: &env.rcoder,
            sid: &sid,
            duration_s: 30.0,
            last_event_id: None,
            idle_stop: true,
        },
    )
    .await;
    let text2 = sse::chunks_text(&evs2);

    // seq 隔离：第二轮流不含第一轮的 chunk 文本（"1"/"2"/"3"/"4" vs "5".."8"）
    let has_turn1_leak = text2.contains("1数到")
        || (text2.contains('1') && text2.contains('4') && !text2.contains('5'));
    report.assert_hard(
        "第二轮不含第一轮内容（seq 隔离）",
        !has_turn1_leak && text2.chars().any(|c| c.is_ascii_digit()),
        format!(
            "turn2 文本: {:?}",
            text2.chars().take(60).collect::<String>()
        ),
    );
    report.diagnostic(
        "两轮文本对照",
        &format!(
            "turn1: {:?} / turn2: {:?}",
            text1.chars().take(40).collect::<String>(),
            text2.chars().take(40).collect::<String>()
        ),
        "chunks_text",
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

#[tokio::test]
async fn userapp_dev_two_turn_isolation_openai() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    scenario_userapp_two_turn_isolation(Backend::Openai).await;
}

#[tokio::test]
async fn userapp_dev_two_turn_isolation_anthropic() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    scenario_userapp_two_turn_isolation(Backend::Anthropic).await;
}
