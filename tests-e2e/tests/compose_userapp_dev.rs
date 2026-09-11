//! compose 环境 Userapp **开发域**场景：per-app 开发容器上的文件转发/拦截分流、
//! PG 凭据对齐、`/computer/chat + service_type=userapp` 开发对话 + SSE 消息流。
//!
//! 运行: `cargo test -p rcoder-e2e --test compose_userapp_dev -- --test-threads=1`
//! （与 compose_sse 同门控：RCODER_URL /health 可达；LLM 场景另需模型配置完整）
//!
//! 覆盖点（对应 rcoder userapp_forward / computer_chat_handler userApp 分支）：
//! - create-workspace 起手（ensure 开发容器 + 建目录 + owner 注册）
//! - X-App-Id 直连转发 + X-Service-Type 拦截分流（两路落同一 workspace）
//! - x-user-id owner 显式档懒创建（无注册前置的拦截分流；502 故障钉住 +
//!   白名单 400——生产 192.168.1.19 cannot resolve owner user_id 回归）
//! - `/api/userapp/db/dev/reset-password`（PG 改密；凭据对齐已内嵌 start 链）
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
    // run_tag 前 6 位=日期：同日重跑（同 tag）会撞容器名（Docker 409，
    // 上轮 cleanup 未达时残留即冲突）——加 pid 段对齐主套件 ident 模式。
    // K8s 边界（229 实测产品 bug）：builder STS pod label 值 =
    // 前缀(19)+app_id+controller-hash(11) 限 63 字节 → app_id 实际上限
    // ~33 字符（远小于 identifier 白名单 64）——tag 压缩到单字母+缩写，
    // 总长 ~26 字符双环境安全
    let short_tag: String = tag
        .split('-')
        .filter_map(|part| part.chars().next())
        .collect();
    format!(
        "e2e-ud-{}-p{}-{}",
        &env.run_tag.replace('_', "")[..6],
        std::process::id() % 1000,
        short_tag
    )
    .chars()
    .take(33)
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
    // 600s：K8s 首次 PVC 动态制备（ceph-rbd 100Gi）常态 ~2 分钟，但删除
    // 风暴后 ceph 恢复期实测可超 300s（229 三轮实测：前轮 PVC Bound 117-
    // 219s，恢复期超时）。post_json 默认 90s 会截断 ensure（pod 实际创建
    // 成功但测试已超时）
    // 冷启动容忍：dev 容器刚建时内嵌 file-server（60000→8086）有 ~10s 启动
    // 窗口，首个 ensure 可能连接层失败（ERR_CONTAINER_ERROR: error sending
    // request）——容器可能已建成功，重试即通（幂等）
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
        .post(format!("{}/api/v1/userapp/generate-file", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user, "file_name": "direct.txt", "content": "via direct"}))
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
            "{}/api/v1/userapp/get-file-list?app_id={app}&user_id={user}",
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

    // 入口 B'：TS 老路径 GET 列表 + 双 header，cId 传会话风格值（≠app_id）——
    // app_id 是独立字段（header 提取），cId（会话字段）不参与 userapp 定位
    let resp = env
        .http
        .get(format!(
            "{}/api/computer/get-file-list?userId={user}&cId=conv-12345&recursive=false",
            env.rcoder
        ))
        .timeout(Duration::from_secs(30))
        .header("X-Service-Type", "userapp")
        .header("X-App-Id", &app)
        .send()
        .await
        .expect("intercepted list");
    let sl_c = resp.status();
    let bl_c: Value = resp.json().await.unwrap_or(Value::Null);
    let names_c: Vec<String> = bl_c["files"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|f| f["name"].as_str().map(str::to_owned))
        .collect();
    let both_c =
        names_c.iter().any(|n| n == "direct.txt") && names_c.iter().any(|n| n == "proxy.txt");
    report.assert_hard(
        "入口 B'：computer 老路径列表，cId 传会话值不参与定位（app_id 独立提取）",
        sl_c.is_success() && both_c,
        format!("HTTP {sl_c}, files: {names_c:?}"),
    );

    // TS 老路径缺 X-App-Id（有 userapp 标记无定位字段）→ 400 fail-fast
    let resp = env
        .http
        .get(format!(
            "{}/api/computer/get-file-list?userId={user}&cId=conv-12345",
            env.rcoder
        ))
        .timeout(Duration::from_secs(15))
        .header("X-Service-Type", "userapp")
        .send()
        .await
        .expect("missing app id list");
    report.assert_hard(
        "computer 老路径缺 X-App-Id → 400",
        resp.status().as_u16() == 400,
        format!("HTTP {}", resp.status()),
    );

    // 缺 X-App-Id 的 userApp 转发 → 400（明确提示）
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/generate-file", env.rcoder))
        .timeout(Duration::from_secs(15))
        .json(&json!({"app_id": app, "user_id": user, "file_name": "x.txt", "content": "x"}))
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
// 场景 2：PG 改密 dev（reset-password；全新容器 PG initdb 有就绪窗口，轮询收敛）
//          首调成功（username upsert 建号）；同密码复调仍成功（upsert 幂等执行）
//          （align-credentials HTTP 入口已下线——start 部署链内嵌对齐）
// ============================================================
#[tokio::test]
async fn userapp_dev_pg_reset_password() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_pg_reset_password";
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
            "/api/v1/userapp/db/dev/reset-password",
            json!({"app_id": app, "user_id": user, "username": "dev", "password": pw}),
        )
        .await;
        if s.is_success() && http_ok(&b) {
            first = Some(b["data"].clone());
            break;
        }
        report.diagnostic(
            "pg reset retry（PG initdb 就绪窗口）",
            &s.to_string(),
            &trunc(&b, 120),
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let ok_first = first.is_some();
    report.assert_hard(
        "首次改密成功（username upsert 建号/改密）",
        ok_first,
        format!("data: {:?}", first.as_ref().map(|d| trunc(d, 80))),
    );
    if !ok_first {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 同密码复调：upsert 幂等执行（ALTER 同密码再次成功，无状态翻转）
    let (_, b2) = post_json(
        &env,
        "/api/v1/userapp/db/dev/reset-password",
        json!({"app_id": app, "user_id": user, "username": "dev", "password": pw}),
    )
    .await;
    let ok_second = http_ok(&b2);
    report.assert_hard(
        "同密码复调仍成功（upsert 幂等执行）",
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
    req.app_id = Some(app.clone());

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
// 场景 3b：userApp 开发对话 agent_work_dir 渗透防御（回归锚点：
//          Java 曾借 agent_work_dir 传会话 ID → agent 落错容器可写层，
//          平台侧按 app_id 找不到代码。修复后定位键恒为 app_id）
// ============================================================
async fn scenario_userapp_chat_workdir_agent_work_dir(backend: Backend) {
    let scenario = "userapp_dev_chat_workdir";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let app = scoped_app(&env, &format!("w-{}", backend.as_str()));
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 模拟 Java 历史行为：agent_work_dir 携带会话 ID（非 app_id）。修复后
    // 网关消毒 + agent_runner 忽略——工作目录恒为 {USERAPP_WORKSPACE_DIR}/{app_id}
    let mut req = env.base_payload(
        backend,
        "创建文件 workdir-landed.txt，内容为 ok。除此之外不要做任何事。",
        &format!("{}-udw", env.run_tag),
        user,
    );
    req.service_type = Some(shared_types::ChatServiceScope::Userapp);
    req.app_id = Some(app.clone());
    req.agent_work_dir = Some("1561845".to_string());

    let Ok(data) = chat_reported(&env, &report, "turn1", &env.rcoder, &req).await else {
        report.assert_hard("chat 成功", false, "chat 失败（见 chat_request 行）".into());
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };
    let sid = data.session_id.clone();
    report.assert_hard("session_id 非空", !sid.is_empty(), sid.clone());
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
            duration_s: 60.0,
            last_event_id: None,
            idle_stop: true,
        },
    )
    .await;
    report.assert_hard(
        "含 end_turn（完整轮）",
        count_event(&events, "end_turn") >= 1,
        format!("事件分布 {}", sse::type_counts(&events)),
    );

    // 核心断言：产物落在平台可见的 workspace——file-server 按
    // {USERAPP_WORKSPACE_DIR}/{app_id} 定位，能列出即证明 agent 落在
    // PVC 挂载点，而非 agent_work_dir 指向的容器可写层目录
    let resp_l = env
        .http
        .get(format!(
            "{}/api/v1/userapp/get-file-list?app_id={app}&user_id={user}",
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
    let landed = names.iter().any(|n| n == "workdir-landed.txt");
    report.assert_hard(
        "agent 产物落 workspace（agent_work_dir=会话ID 不再劫持定位）",
        landed,
        format!("HTTP {sl}, files: {names:?}"),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

#[tokio::test]
async fn userapp_dev_chat_workdir_agent_work_dir_openai() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    scenario_userapp_chat_workdir_agent_work_dir(Backend::Openai).await;
}

#[tokio::test]
async fn userapp_dev_chat_workdir_agent_work_dir_anthropic() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    scenario_userapp_chat_workdir_agent_work_dir(Backend::Anthropic).await;
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
        r.app_id = Some(app.clone());
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

// ============================================================
// B1 归档下载族：zip-workspace / download-all-files（PK 魔数 + 兜底差异）
// ============================================================
#[tokio::test]
async fn userapp_dev_archive_downloads() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_archive";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "zip");
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 造内容
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/generate-file", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user, "file_name": "zip-probe.txt", "content": "zip-probe-content"}))
        .send()
        .await
        .expect("generate");
    report.assert_hard(
        "归档前置：generate-file 造内容",
        resp.status().is_success(),
        format!("HTTP {}", resp.status()),
    );

    // zip-workspace：application/zip + PK 魔数 + Content-Disposition
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/zip-workspace", env.rcoder))
        .timeout(Duration::from_secs(60))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user}))
        .send()
        .await
        .expect("zip post");
    let status = resp.status();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let cd = resp
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = resp.bytes().await.unwrap_or_default();
    let zip_ok = status.is_success()
        && ct.contains("application/zip")
        && cd.contains(&format!("{user}_{app}"))
        && bytes.len() > 4
        && bytes[..2] == *b"PK";
    report.assert_hard(
        "zip-workspace → zip 流（PK 魔数 + Content-Disposition 文件名）",
        zip_ok,
        format!("HTTP {status}, ct={ct}, cd={cd}, {} bytes", bytes.len()),
    );

    // download-all-files：顶层前缀 + 同魔数
    let resp = env
        .http
        .get(format!(
            "{}/api/v1/userapp/download-all-files?app_id={app}&user_id={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(60))
        .header("X-App-Id", &app)
        .send()
        .await
        .expect("download get");
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    report.assert_hard(
        "download-all-files → zip 流（PK 魔数）",
        status.is_success() && bytes.len() > 4 && bytes[..2] == *b"PK",
        format!("HTTP {status}, {} bytes", bytes.len()),
    );

    // 兜底差异：workspace 不存在——zip-workspace 404 / download-all-files 空 zip 200。
    // 用"容器在 + destroy dev storage（目录已删）"构造：ghost app 透传 ensure 新建
    // 容器后 file-server 有启动窗口，无就绪退避会 502（实现差距已记录在案）
    drop(
        post_json(
            &env,
            &format!("/api/v1/userapp/{app}/dev/storage/destroy"),
            json!({"user_id": user, "confirm": app}),
        )
        .await,
    );
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/zip-workspace", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user}))
        .send()
        .await
        .expect("destroyed zip");
    // 实测发现：zip-workspace 的 404 分支不可达——resolve_userapp_dev 有
    // create_dir_all 副作用，透传到达前 workspace 根已被幂等重建（恒 200 空 zip）
    report.assert_hard(
        "zip-workspace workspace 已 destroy → 200（resolve 幂等重建，404 不可达）",
        resp.status().is_success(),
        format!("HTTP {}", resp.status()),
    );
    let resp = env
        .http
        .get(format!(
            "{}/api/v1/userapp/download-all-files?app_id={app}&user_id={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .send()
        .await
        .expect("destroyed download");
    let dl_status = resp.status();
    let dl_bytes = resp.bytes().await.unwrap_or_default();
    report.assert_hard(
        "download-all-files 目录不存在 → 空 zip 兜底（200 + PK）",
        dl_status.is_success() && dl_bytes.len() > 4 && dl_bytes[..2] == *b"PK",
        format!("HTTP {dl_status}, {} bytes", dl_bytes.len()),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// B2 push-skills：multipart skill zip → updated_skills + 落盘
// ============================================================
#[tokio::test]
async fn userapp_dev_skills_push() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_skills";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "skl");
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 构造 skill zip（e2e-skill-probe/SKILL.md）
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
    zw.start_file("skills/e2e-skill-probe/SKILL.md", opts)
        .unwrap();
    std::io::Write::write_all(&mut zw, b"---\nname: e2e-skill-probe\n---\nprobe\n").unwrap();
    let zip_bytes = zw.finish().unwrap().into_inner();

    let part = reqwest::multipart::Part::bytes(zip_bytes).file_name("skills.zip");
    let form = reqwest::multipart::Form::new()
        .text("app_id", app.clone())
        .text("user_id", user.to_owned())
        .part("file", part);
    let resp = env
        .http
        .post(format!(
            "{}/api/v1/userapp/push-skills-to-workspace",
            env.rcoder
        ))
        .timeout(Duration::from_secs(60))
        .header("X-App-Id", &app)
        .multipart(form)
        .send()
        .await
        .expect("skills post");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let pushed = status.is_success()
        && body["success"].as_bool() == Some(true)
        && body["updated_skills"]
            .as_array()
            .is_some_and(|arr| arr.iter().any(|s| s == "e2e-skill-probe"));
    report.assert_hard(
        "push-skills → updated_skills 含探针 skill",
        pushed,
        format!("HTTP {status}, body 截断: {}", trunc(&body, 150)),
    );

    // 落盘复核：get-file-list 看 .agents/skills
    let resp = env
        .http
        .get(format!(
            "{}/api/v1/userapp/get-file-list?app_id={app}&user_id={user}&recursive=false&relative_path=.agents/skills",
            env.rcoder
        ))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .send()
        .await
        .expect("list skills");
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let landed = body["files"].as_array().is_some_and(|files| {
        files.iter().any(|f| {
            f["name"]
                .as_str()
                .is_some_and(|n| n.contains("e2e-skill-probe"))
        })
    });
    report.assert_hard(
        "push-skills 落盘 .agents/skills/<name>",
        landed,
        format!("body 截断: {}", trunc(&body, 150)),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// B3 模板 zip 上传 + projects detect/confirm 门面
// ============================================================
#[tokio::test]
async fn userapp_dev_template_zip_and_projects() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_tpl_zip";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "tpl");
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 最小模板 zip：workspace.manifest.toml + backend-go go 特征。
    // 不放正式 project.manifest.toml——detect 对已 confirm 的项目 400
    // "already has a confirmed manifest"；只放特征让 detect 生成 draft
    let ws_manifest = "schema_version = 1\n\n[workspace]\nname = \"e2e-tpl\"\n";
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
    zw.start_file("workspace.manifest.toml", opts).unwrap();
    std::io::Write::write_all(&mut zw, ws_manifest.as_bytes()).unwrap();
    zw.start_file("backend-go/go.mod", opts).unwrap();
    std::io::Write::write_all(&mut zw, b"module e2e/tpl\n\ngo 1.26\n").unwrap();
    zw.start_file("backend-go/main.go", opts).unwrap();
    std::io::Write::write_all(&mut zw, b"package main\n\nfunc main() {}\n").unwrap();
    let zip_bytes = zw.finish().unwrap().into_inner();

    let part = reqwest::multipart::Part::bytes(zip_bytes).file_name("template.zip");
    let form = reqwest::multipart::Form::new()
        .text("app_id", app.clone())
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
        .header("X-App-Id", &app)
        .multipart(form)
        .send()
        .await
        .expect("init zip");
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let init_ok = status.is_success()
        && body["success"].as_bool() == Some(true)
        && body["workspace_root"]
            .as_str()
            .is_some_and(|w| !w.is_empty());
    report.assert_hard(
        "init-project-template（zip 上传形态）→ workspace_root",
        init_ok,
        format!("HTTP {status}, body 截断: {}", trunc(&body, 150)),
    );
    if !init_ok {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // projects/detect
    let (ds, db) = post_json(
        &env,
        &format!("/api/v1/userapp/{app}/dev/projects/detect"),
        json!({"user_id": user, "project_dir": "backend-go"}),
    )
    .await;
    let detect_ok = ds.is_success()
        && http_ok(&db)
        && db["data"]["detection"]["detected_type"]
            .as_str()
            .is_some_and(|t| !t.is_empty());
    report.assert_hard(
        "projects/detect → detected_type 非空",
        detect_ok,
        format!("HTTP {ds}, body 截断: {}", trunc(&db, 180)),
    );

    // projects/confirm + 幂等
    let (c1s, c1b) = post_json(
        &env,
        &format!("/api/v1/userapp/{app}/dev/projects/confirm"),
        json!({"user_id": user, "project_dir": "backend-go"}),
    )
    .await;
    let confirm_ok = c1s.is_success()
        && http_ok(&c1b)
        && c1b["data"]["path"].as_str().is_some_and(|p| !p.is_empty());
    report.assert_hard(
        "projects/confirm → data.path 非空",
        confirm_ok,
        format!("HTTP {c1s}, body 截断: {}", trunc(&c1b, 150)),
    );
    if confirm_ok {
        // confirm 是一次性状态迁移（draft rename 为正式 manifest）——二次
        // 必然拒绝（draft 已不存在），与 detect 的 already-confirmed 语义自洽
        let (c2s, c2b) = post_json(
            &env,
            &format!("/api/v1/userapp/{app}/dev/projects/confirm"),
            json!({"user_id": user, "project_dir": "backend-go"}),
        )
        .await;
        let second_rejected = !http_ok(&c2b) || c2b["data"]["path"].as_str().is_none();
        report.assert_hard(
            "projects/confirm 二次 → 拒绝（一次性状态迁移）",
            second_rejected,
            format!("HTTP {c2s}, body 截断: {}", trunc(&c2b, 120)),
        );
    }

    // prod stage → 400（dev-only 能力）
    let (ps, _) = post_json(
        &env,
        &format!("/api/v1/userapp/{app}/prod/projects/detect"),
        json!({"user_id": user, "project_dir": "backend-go"}),
    )
    .await;
    report.assert_hard(
        "projects/detect stage=prod → 400（dev-only）",
        ps.as_u16() == 400,
        format!("HTTP {ps}"),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// B4 dev/dbx 代理（Pingora 8089 真实代理面）
// ============================================================
#[tokio::test]
async fn userapp_dev_dbx_proxy() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_dbx";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "dbx");
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    let pingora = std::env::var("E2E_PINGORA_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8089".to_owned());

    // B4b 无尾斜杠规范化：dbx-web 是相对路径 SPA（index.html ./assets/...），浏览器
    // 基准目录由 URL 尾斜杠决定——无尾斜杠时 {app_id} 被当文件名、./assets 向上
    // 错位一级 → 静态资源 404 白屏；request_filter 对"恰好停在 {app_id} 的 dbx
    // 入口"307 到同路径 + /（query 保留）。307 短路是纯路由判定，不依赖容器
    // 就绪，先于就绪轮询断言。
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("no-redirect client");
    let root_url = format!("{pingora}/api/v1/userapp/proxy/dbx/dev/{user}/{app}");
    let resp307 = no_redirect
        .get(&root_url)
        .timeout(Duration::from_secs(15))
        .send()
        .await;
    let location = resp307.as_ref().ok().and_then(|r| {
        r.headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    });
    report.assert_hard(
        "Pingora dev/dbx 无尾斜杠 → 307 + 相对 Location 带斜杠",
        matches!(&resp307, Ok(r) if r.status() == reqwest::StatusCode::TEMPORARY_REDIRECT)
            && location.as_deref()
                == Some(format!("/api/v1/userapp/proxy/dbx/dev/{user}/{app}/").as_str()),
        match &resp307 {
            Ok(r) => format!("HTTP {} Location={location:?}", r.status()),
            Err(e) => format!("err: {e}"),
        },
    );

    // query 原样保留（SPA 侧状态参数随重定向不丢）
    let resp_q = no_redirect
        .get(format!("{root_url}?tab=1"))
        .timeout(Duration::from_secs(15))
        .send()
        .await;
    let loc_q = resp_q.as_ref().ok().and_then(|r| {
        r.headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    });
    report.assert_hard(
        "Pingora dev/dbx 307 query 保留",
        matches!(&resp_q, Ok(r) if r.status() == reqwest::StatusCode::TEMPORARY_REDIRECT)
            && loc_q.as_deref()
                == Some(format!("/api/v1/userapp/proxy/dbx/dev/{user}/{app}/?tab=1").as_str()),
        match &resp_q {
            Ok(r) => format!("HTTP {} Location={loc_q:?}", r.status()),
            Err(e) => format!("err: {e}"),
        },
    );

    // Pingora dev/dbx → builder 容器 dbx-web 4224 GUI 页。
    // 就绪轮询而非单发：ensure 刚建的开发容器里 dbx-web 恒起需 ~10s（产品行为
    // =首访 502 秒级拉回、二访 200），单发断言在冷环境（dev-hot restart 后）必
    // 撞 502 时序竞态；轮询只放宽就绪窗口，200 断言口径不变
    let dbx_url = format!("{pingora}/api/v1/userapp/proxy/dbx/dev/{user}/{app}/");
    let deadline = Instant::now() + Duration::from_secs(30);
    let resp = loop {
        let r = env
            .http
            .get(&dbx_url)
            .timeout(Duration::from_secs(15))
            .send()
            .await;
        let ok = matches!(&r, Ok(r) if r.status().is_success());
        if ok || Instant::now() >= deadline {
            break r;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let ok = matches!(&resp, Ok(r) if r.status().is_success());
    report.assert_hard(
        "Pingora dev/dbx → 200（builder dbx-web GUI）",
        ok,
        match &resp {
            Ok(r) => format!("HTTP {}", r.status()),
            Err(e) => format!("err: {e}"),
        },
    );

    // B4c 浏览器行为模拟：默认跟随重定向的 client 访问无尾斜杠 → 一次 307 后
    // 最终 200 且 URL 收敛到带斜杠形态（相对路径基准正确，白屏场景根治）
    let follow = env
        .http
        .get(&root_url)
        .timeout(Duration::from_secs(15))
        .send()
        .await;
    report.assert_hard(
        "Pingora dev/dbx 无尾斜杠跟随重定向 → 200 + URL 收敛带斜杠",
        matches!(&follow, Ok(r) if r.status().is_success() && r.url().path().ends_with('/')),
        match &follow {
            Ok(r) => format!("HTTP {} final={}", r.status(), r.url()),
            Err(e) => format!("err: {e}"),
        },
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// B5 dev server 进程族：backend-go 单服务 → dev/start → 9080 探活 → stop
// ============================================================
#[tokio::test]
async fn userapp_dev_server_lifecycle() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_server";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    // logs/query 等接口要求 app_id 整体以 "app-" 开头（scoped_app 产物
    // 以 run_tag 开头不满足）——本场景自拼合规名（app- + tag 压缩段）
    let app = format!("app-{}", scoped_app(&env, "srv"));
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 模板：backend-go 单服务 zip（含正式 manifest——dev/start 直接编排）
    // build = zip 打包 start.sh（秒过且产物是合法 zip——touch 空文件过得了
    // 存在性校验但挂产物 parse）；run = http.server serve 服务目录（touch ready
    // 文件使 GET /ready 命中 200 → readiness 探测通过 → 任务正向 Completed）
    let ws_manifest = "schema_version = 1\n\n[workspace]\nname = \"e2e-srv\"\n";
    let proj_manifest = "schema_version = 1\n\n[project]\nservice_id = \"backend-go\"\nname = \"Go Backend\"\ntype = \"go\"\nkind = \"web\"\nenabled = true\n\n[build]\ncommand = [\"sh\", \"-c\", \"zip -q artifact.zip start.sh\"]\nartifact = \"artifact.zip\"\n\n[run]\ncommand = [\"sh\", \"-c\", \"touch ready && exec python3 -m http.server $PORT --bind 0.0.0.0\"]\n\n[health]\nreadiness_path = \"/ready\"\n\n[proxy]\npath = \"/api/go/\"\nstrip_prefix = true\n";
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
    zw.start_file("workspace.manifest.toml", opts).unwrap();
    std::io::Write::write_all(&mut zw, ws_manifest.as_bytes()).unwrap();
    zw.start_file("backend-go/project.manifest.toml", opts)
        .unwrap();
    std::io::Write::write_all(&mut zw, proj_manifest.as_bytes()).unwrap();
    zw.start_file("backend-go/server", opts).unwrap();
    // 最小静态 busybox 风格占位二进制不可行——用 sh 脚本替代（exec 权限 zip
    // 里无法设置，dev/start 的 spawn command 需要 exec 位……实际由 app-cli
    // 经 shell？查证：run.command 直接 exec。改用 go 编译太重——用 /bin/sh
    // 脚本 + zip 外部 chmod 不行。方案：manifest run command 用 ["sh","-c","sleep 9999"]
    zw.start_file("backend-go/start.sh", opts).unwrap();
    std::io::Write::write_all(&mut zw, b"#!/bin/sh\nsleep 9999\n").unwrap();
    let zip_bytes = zw.finish().unwrap().into_inner();

    let part = reqwest::multipart::Part::bytes(zip_bytes).file_name("template.zip");
    let form = reqwest::multipart::Form::new()
        .text("app_id", app.clone())
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
        .header("X-App-Id", &app)
        .multipart(form)
        .send()
        .await
        .expect("init zip");
    report.assert_hard(
        "dev server 前置：init 模板 zip",
        resp.status().is_success(),
        format!("HTTP {}", resp.status()),
    );

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
        "dev/start 受理（task_id + pending）",
        status.is_success() && http_ok(&body) && !task_id.is_empty(),
        format!("HTTP {status}, body 截断: {}", trunc(&body, 150)),
    );
    if task_id.is_empty() {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 轮询任务到终态（免编译应秒级）
    let mut terminal = None;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(120) {
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
                b["data"]["error"].as_str().unwrap_or("").to_string(),
            ));
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let (term_ok, err) = match &terminal {
        Some((st, e)) => (st == "completed", e.clone()),
        None => (false, "120s 未到终态".into()),
    };
    report.assert_hard(
        "dev/start 任务 completed",
        term_ok,
        format!("terminal={terminal:?}, err: {err}"),
    );

    // dev/list：port=9080 + pid>0
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

    // zip 部署落盘断言：dev 运行源=制品解压目录 .run（与生产部署物一致）——
    // resolve-file 探测 .run 内 workspace 清单存在即证解压换入成功
    let resp = env
        .http
        .get(format!(
            "{}/api/v1/userapp/resolve-file?app_id={app}&user_id={user}&file_path=.run/workspace.manifest.toml",
            env.rcoder
        ))
        .timeout(Duration::from_secs(15))
        .header("X-App-Id", &app)
        .send()
        .await
        .expect("resolve run dir");
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let run_ok = body["success"].as_bool() == Some(true) && body["exists"].as_bool() == Some(true);
    report.assert_hard(
        "dev zip 部署 → .run/workspace.manifest.toml 存在",
        run_ok,
        format!("body 截断: {}", trunc(&body, 150)),
    );

    // 编排日志内置源：logs/query 按 service_id=app-cli 过滤（dev/logs 已下线，
    // app-cli 自身编排日志由日志族接口内置源统一提供）
    let resp = env
        .http
        .post(format!(
            "{}/api/v1/userapp/{app}/dev/logs/query?user_id={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(15))
        .json(&json!({"selectors": [{"service_id": "app-cli"}]}))
        .send()
        .await
        .expect("orchestrator logs query");
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let logs_ok = body["data"]["logs"].as_array().is_some_and(|arr| {
        arr.iter()
            .any(|log| log["service_id"] == "app-cli" && log["source_id"] == "orchestrator")
    });
    report.assert_hard(
        "logs/query orchestrator 源 → app-cli 编排日志可见",
        logs_ok,
        format!("body 截断: {}", trunc(&body, 120)),
    );

    // dev/stop → list 空
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
        .expect("dev list after stop");
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "dev/stop 后 list 空",
        body["data"]["list"]
            .as_array()
            .is_some_and(|arr| arr.is_empty()),
        format!("body 截断: {}", trunc(&body, 120)),
    );

    // dev 停止后查 dev 日志 → 受理前置检查快速失败（400 ERR_DEV_NOT_RUNNING），
    // 不再是挂满连接超时后的 500 ERR_BACKEND_ERROR（app-cli :3010 随会话退出）
    let resp = env
        .http
        .post(format!(
            "{}/api/v1/userapp/{app}/dev/logs/sources/query?user_id={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(10))
        .json(&json!({}))
        .send()
        .await
        .expect("logs sources query after dev stop");
    let stopped_status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "dev/stop 后 logs/sources/query → 快速 400 ERR_DEV_NOT_RUNNING",
        stopped_status.as_u16() == 400 && body["code"].as_str() == Some("ERR_DEV_NOT_RUNNING"),
        format!("status={stopped_status}, body 截断: {}", trunc(&body, 120)),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// 场景 5：agent 族接口 userApp 分派（service_type=userapp +
// project_id 兼任 app_id + app_stage 缺省 dev）
// ============================================================
/// 分派正向链：dev chat 建会话 → status 分派（is_alive + session 回显）→
/// cancel 分派（幂等 success）→ stop 分派 → cache-clean 分派（owner 显式）
/// → computer 旧形态回归（不带 service_type 不炸）。
async fn scenario_userapp_agent_dispatch(backend: Backend) {
    let scenario = "userapp_agent_dispatch";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let app = scoped_app(&env, &format!("ad-{}", backend.as_str()));
    let user = "e2e-ud-user";

    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // dev chat 建会话（分派的目标会话；短 prompt 控制时长）
    let mut req = env.base_payload(backend, "只回复 ok", &format!("{}-adp", env.run_tag), user);
    req.service_type = Some(shared_types::ChatServiceScope::Userapp);
    req.app_id = Some(app.clone());
    let sid = match chat_reported(&env, &report, "dispatch_chat", &env.rcoder, &req).await {
        Ok(d) if !d.session_id.is_empty() => d.session_id,
        _ => {
            report.assert_hard("chat 建会话", false, "chat 失败".into());
            assert_hard_all(report).await;
            cleanup_builder(&app);
            return;
        }
    };
    report.assert_hard("chat 建会话", true, sid.clone());
    // 等 turn 收尾（会话映射在响应后写入；status 读取依赖其落库）
    tokio::time::sleep(Duration::from_secs(6)).await;

    // 通用 POST（信封解析；五接口全部 HTTP 200 + code/success 判定）
    async fn post(env: &Env, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        let resp = env
            .http
            .post(format!("{}/{path}", env.rcoder))
            .timeout(Duration::from_secs(120))
            .json(&body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{path} post: {e}"));
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    // ① status 分派：is_alive=true + 会话回显（映射+GetStatus 双确认）
    let (st, body) = post(
        &env,
        "computer/agent/status",
        json!({"service_type": "userapp", "app_id": app}),
    )
    .await;
    let ok = st.is_success()
        && http_ok(&body)
        && body["data"]["is_alive"].as_bool() == Some(true)
        && body["data"]["session_id"].as_str() == Some(sid.as_str());
    report.assert_hard(
        "status 分派 alive+session 回显",
        ok,
        format!("HTTP {st}, {}", trunc(&body, 160)),
    );

    // ② cancel 分派：幂等 success（会话已结束或仍活跃均成功）
    let (st, body) = post(
        &env,
        "computer/agent/session/cancel",
        json!({"service_type": "userapp", "app_id": app, "session_id": sid}),
    )
    .await;
    let ok = st.is_success() && http_ok(&body) && body["data"]["success"].as_bool() == Some(true);
    report.assert_hard(
        "cancel 分派 success",
        ok,
        format!("HTTP {st}, {}", trunc(&body, 160)),
    );

    // ③ stop 分派：停掉 app 会话的 agent（builder 容器继续运行）
    let (st, body) = post(
        &env,
        "computer/agent/stop",
        json!({"service_type": "userapp", "app_id": app}),
    )
    .await;
    let ok = st.is_success() && http_ok(&body) && body["data"]["success"].as_bool() == Some(true);
    report.assert_hard(
        "stop 分派 success",
        ok,
        format!("HTTP {st}, {}", trunc(&body, 160)),
    );

    // ④ cache-clean 分派：owner 显式传（清 dev 工作区 .cache，幂等）
    let (st, body) = post(
        &env,
        "computer/cache/clean",
        json!({"service_type": "userapp", "app_id": app, "user_id": user}),
    )
    .await;
    let ok = st.is_success() && http_ok(&body);
    report.assert_hard(
        "cache-clean 分派 success",
        ok,
        format!("HTTP {st}, {}", trunc(&body, 160)),
    );

    // ⑤ computer 旧形态回归：不带 service_type 走原路径（信封完整不炸）
    let (st, body) = post(
        &env,
        "computer/agent/status",
        json!({"user_id": user, "project_id": app}),
    )
    .await;
    let ok = st.is_success() && body["success"].is_boolean();
    report.assert_hard(
        "computer 旧形态回归（无 service_type）",
        ok,
        format!("HTTP {st}, {}", trunc(&body, 160)),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

#[tokio::test]
async fn userapp_agent_dispatch_openai() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    scenario_userapp_agent_dispatch(Backend::Openai).await;
}

#[tokio::test]
async fn userapp_agent_dispatch_anthropic() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    scenario_userapp_agent_dispatch(Backend::Anthropic).await;
}

// ============================================================
// 场景 6：x-user-id owner 显式档懒创建（生产故障回归）
//   192.168.1.19 实测断链：Java computer 文件族 + X-Service-Type/X-App-Id
//   分流、无注册前置也无 x-user-id 时，透传层 owner 三档两档皆空 →
//   502 "cannot resolve owner user_id"（fail-fast 防宿主树孤儿目录）。
//   修复（d9e208b）：拦截层从 x-user-id header 提取 owner 显式档。
// ============================================================
#[tokio::test]
async fn userapp_dev_owner_header_lazy_ensure() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_owner";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "o1");
    let user = "e2e-ud-ohuser";

    // A｜复现生产故障：全新 app（无注册、无 x-user-id）→ 502 cannot resolve
    //    （不走 create-workspace——正是生产 Java 的接入形态）
    let resp = env
        .http
        .post(format!("{}/api/computer/generate-file", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-Service-Type", "userapp")
        .header("X-App-Id", &app)
        .json(&json!({"userId": user, "cId": app, "fileName": "a.txt", "content": "x"}))
        .send()
        .await
        .expect("no-owner post");
    let status_a = resp.status();
    let body_a: Value = resp.json().await.unwrap_or(Value::Null);
    let ok_a = status_a.as_u16() == 502
        && body_a["message"]
            .as_str()
            .is_some_and(|m| m.contains("cannot resolve owner user_id for app"));
    report.assert_hard(
        "A：无 owner 懒创建拒（502 cannot resolve——fail-fast 防孤儿目录，生产行为钉住）",
        ok_a,
        format!("HTTP {status_a}, {}", trunc(&body_a, 160)),
    );

    // B｜x-user-id 显式档：同请求补 header → 懒创建 + 透传成功。
    //    重试幂等（首次含容器拉起 + 容器内 file-server ~10s 启动窗口）
    let mut ok_b = false;
    let (mut status_b, mut body_b) = (status_a, body_a.clone());
    for attempt in 0..3 {
        let resp = env
            .http
            .post(format!("{}/api/computer/generate-file", env.rcoder))
            .timeout(Duration::from_secs(120))
            .header("X-Service-Type", "userapp")
            .header("X-App-Id", &app)
            .header("X-User-Id", user)
            .json(&json!({"userId": user, "cId": app, "fileName": "b.txt", "content": "via x-user-id"}))
            .send()
            .await
            .expect("owner post");
        status_b = resp.status();
        body_b = resp.json().await.unwrap_or(Value::Null);
        if status_b.is_success() && body_b["success"].as_bool() == Some(true) {
            ok_b = true;
            break;
        }
        report.diagnostic(
            "x-user-id 懒创建冷启动重试（file-server 启动窗口）",
            &format!("attempt {attempt}"),
            &trunc(&body_b, 100),
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    report.assert_hard(
        "B：x-user-id 显式档懒创建成功（拦截分流透传 200）",
        ok_b,
        format!("HTTP {status_b}, {}", trunc(&body_b, 120)),
    );

    // B'｜懒创建后注册表命中：无 header 再调同 app → 200（owner 只在创建路径需要）
    let resp = env
        .http
        .post(format!("{}/api/computer/generate-file", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-Service-Type", "userapp")
        .header("X-App-Id", &app)
        .json(&json!({"userId": user, "cId": app, "fileName": "b2.txt", "content": "reuse"}))
        .send()
        .await
        .expect("registry hit post");
    let status_r = resp.status();
    let body_r: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "B'：懒创建后注册表命中（无 header 复用 200——owner 仅创建路径需要）",
        status_r.is_success() && body_r["success"].as_bool() == Some(true),
        format!("HTTP {status_r}, {}", trunc(&body_r, 120)),
    );

    // C｜白名单：非法 x-user-id（路径逃逸形态）→ 400（防宿主树拼接逃逸）
    let resp = env
        .http
        .post(format!("{}/api/computer/generate-file", env.rcoder))
        .timeout(Duration::from_secs(15))
        .header("X-Service-Type", "userapp")
        .header("X-App-Id", &app)
        .header("X-User-Id", "../escape")
        .json(&json!({"fileName": "c.txt", "content": "x"}))
        .send()
        .await
        .expect("bad uid post");
    let status_c = resp.status();
    let body_c: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "C：非法 x-user-id → 400（identifier 白名单防逃逸）",
        status_c.as_u16() == 400
            && body_c["message"]
                .as_str()
                .is_some_and(|m| m.contains("user_id")),
        format!("HTTP {status_c}, {}", trunc(&body_c, 120)),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// 场景 7：新接口族 body/query 自定位（无 header 依赖）
//   生产对接断链（testagent 400 missing x-app-id）：Java 直连新 userApp
//   接口（dev server 生命周期/build/ensure-workspace）定位参数自携带，
//   header 契约保留给 TS 老族（file-server-proxy 反代形态）。
// ============================================================
#[tokio::test]
async fn userapp_dev_new_endpoint_body_query_locate() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_nep";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = scoped_app(&env, "n1");
    let user = "e2e-ud-nepuser";

    // A｜body-only POST dev/restart：无任何 X- header，body snake_case 自定位
    //    （含懒创建——body user_id 即 owner 显式档）。重试幂等（容器冷启动窗口）
    let mut ok_a = false;
    let (mut status_a, mut body_a) = (reqwest::StatusCode::BAD_REQUEST, Value::Null);
    for attempt in 0..3 {
        let resp = env
            .http
            .post(format!("{}/api/v1/userapp/dev/restart", env.rcoder))
            .timeout(Duration::from_secs(120))
            .json(&json!({"app_id": app, "user_id": user}))
            .send()
            .await
            .expect("body-located post");
        status_a = resp.status();
        body_a = resp.json().await.unwrap_or(Value::Null);
        if status_a.is_success() && http_ok(&body_a) && body_a["data"]["task_id"].is_string() {
            ok_a = true;
            break;
        }
        report.diagnostic(
            "body 定位懒创建冷启动重试",
            &format!("attempt {attempt}"),
            &trunc(&body_a, 100),
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    report.assert_hard(
        "A：body-only dev/restart 自定位（零 header，懒创建+任务受理）",
        ok_a,
        format!("HTTP {status_a}, {}", trunc(&body_a, 140)),
    );

    // B｜query-only GET dev/list：无 header，query app_id+user_id 自定位
    let resp = env
        .http
        .get(format!(
            "{}/api/v1/userapp/dev/list?app_id={app}&user_id={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("query-located get");
    let status_b = resp.status();
    let body_b: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "B：query-only dev/list 自定位（零 header，list 信封可达）",
        status_b.is_success() && http_ok(&body_b) && body_b["data"]["list"].is_array(),
        format!("HTTP {status_b}, {}", trunc(&body_b, 120)),
    );

    // C｜camelCase body 不被识别（契约不沾染 Java DTO）→ 400 双来源指引
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/dev/restart", env.rcoder))
        .timeout(Duration::from_secs(15))
        .json(&json!({"appId": app, "userId": user}))
        .send()
        .await
        .expect("camelCase post");
    let status_c = resp.status();
    let body_c: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "C：camelCase body 不识别 → 400（指引 X-App-Id header 或 body app_id 字段）",
        status_c.as_u16() == 400
            && body_c["message"]
                .as_str()
                .is_some_and(|m| m.contains("missing app_id")),
        format!("HTTP {status_c}, {}", trunc(&body_c, 120)),
    );

    // D｜老族 header 契约不变：TS 老路径无 X-App-Id 仍拒（file-server-proxy 形态保留）
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/get-file-list", env.rcoder))
        .timeout(Duration::from_secs(15))
        .header("X-App-Id", &app)
        .json(&json!({}))
        .send()
        .await
        .expect("legacy header post");
    // 带 header 的老族调用可达（容器侧自答参数校验）——不带则 400 missing header
    let resp2 = env
        .http
        .post(format!("{}/api/v1/userapp/get-file-list", env.rcoder))
        .timeout(Duration::from_secs(15))
        .json(&json!({}))
        .send()
        .await
        .expect("legacy no-header post");
    let status_d = resp2.status();
    let body_d: Value = resp2.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "D：TS 老族 header 必填不变（无 X-App-Id 仍 400——反代契约保留）",
        status_d.as_u16() == 400
            && body_d["message"]
                .as_str()
                .is_some_and(|m| m.contains("x-app-id")),
        format!("HTTP {status_d}（带 header 对照 HTTP {}）", resp.status()),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// 场景：workspace 就绪性前置校验负路径——dev/start 受理期 4xx 快速失败
//       （不创建任务）+ ERR_DEV_NOT_RUNNING 未启动互补路径。无 LLM 依赖。
//       锚定 file-server-userapp precheck_dev_workspace（94329c5）：
//       空 workspace → ERR_WORKSPACE_EMPTY；有文件无 manifest →
//       ERR_WORKSPACE_NO_SERVICES；logs 族未启动 → ERR_DEV_NOT_RUNNING。
// ============================================================
#[tokio::test]
async fn userapp_dev_precheck_rejects_empty_and_no_services() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_precheck";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    // logs 族接口沿用 app- 前缀形态（lifecycle 场景同款自拼，最稳）
    let app = format!("app-{}", scoped_app(&env, "prechk"));
    let user = "e2e-ud-user";

    // create-workspace 只建空目录（file-server ensure_workspace 仅 create_dir_all）
    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 1. 空 workspace → dev/start 受理期拒绝（400 + 无 task_id = 不创建任务）
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/dev/start", env.rcoder))
        .timeout(Duration::from_secs(90))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user}))
        .send()
        .await
        .expect("dev start on empty workspace");
    let s1 = resp.status();
    let b1: Value = resp.json().await.unwrap_or(Value::Null);
    let no_task = b1["data"]["task_id"].as_str().is_none_or(str::is_empty);
    report.assert_hard(
        "空 workspace → dev/start 400 ERR_WORKSPACE_EMPTY（不创建任务）",
        s1.as_u16() == 400 && b1["code"].as_str() == Some("ERR_WORKSPACE_EMPTY") && no_task,
        format!("HTTP {s1}, {}", trunc(&b1, 120)),
    );

    // 2. 写一个普通文件（非 manifest）→ 非空但 discover 无 enabled 服务
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/generate-file", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user, "file_name": "probe.txt", "content": "no manifest"}))
        .send()
        .await
        .expect("generate probe file");
    let s2 = resp.status();
    let b2: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "构造前提：generate-file 写入 probe.txt 成功",
        s2.is_success() && b2["success"].as_bool() == Some(true),
        format!("HTTP {s2}, {}", trunc(&b2, 100)),
    );

    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/dev/start", env.rcoder))
        .timeout(Duration::from_secs(90))
        .header("X-App-Id", &app)
        .json(&json!({"app_id": app, "user_id": user}))
        .send()
        .await
        .expect("dev start without manifest");
    let s3 = resp.status();
    let b3: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "无 manifest workspace → dev/start 400 ERR_WORKSPACE_NO_SERVICES",
        s3.as_u16() == 400 && b3["code"].as_str() == Some("ERR_WORKSPACE_NO_SERVICES"),
        format!("HTTP {s3}, {}", trunc(&b3, 120)),
    );

    // 3. 从未 dev/start 的 workspace 查 dev 日志 → 未启动互补路径（与 lifecycle
    //    场景的 stop 后断言互补：判定只看 dev/list 空，不区分从未启动/已停止）
    let resp = env
        .http
        .post(format!(
            "{}/api/v1/userapp/{app}/dev/logs/sources/query?user_id={user}",
            env.rcoder
        ))
        .timeout(Duration::from_secs(10))
        .json(&json!({}))
        .send()
        .await
        .expect("logs query never started");
    let s4 = resp.status();
    let b4: Value = resp.json().await.unwrap_or(Value::Null);
    report.assert_hard(
        "从未 dev/start → logs/sources/query 400 ERR_DEV_NOT_RUNNING",
        s4.as_u16() == 400 && b4["code"].as_str() == Some("ERR_DEV_NOT_RUNNING"),
        format!("HTTP {s4}, {}", trunc(&b4, 120)),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

/// docker inspect 读容器 Id（cleanup_builder 同款 std::process::Command 先例）。
fn docker_inspect_id(app_id: &str) -> Option<String> {
    let name = format!("rcoder-app-builder-{app_id}");
    let out = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", &name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
}

// ============================================================
// 场景：builder 注册表自愈——docker restart 使 IP 变化（注册表脏 IP），
//       转发链探活失败 → remediate 以容器真实状态裁决 → Running 保容器
//       刷新注册（container_id 不变），而非杀重建。锚定 94329c5 的
//       remediate_stale_registry / refreshed_registration。
//       Docker 模式专属（K8s STS 重建语义不同构，由部署验证覆盖）。
// ============================================================
#[tokio::test]
async fn userapp_dev_registry_self_heal_after_restart() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_heal";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    if !env.k8s_ssh.is_empty() {
        eprintln!("[{scenario}] K8s 模式跳过（Docker 注册表自愈专属场景）");
        return;
    }
    let app = format!("app-{}", scoped_app(&env, "heal"));
    let user = "e2e-ud-user";

    // create-workspace 后不再调 get-file-list——避免探活正缓存（PROBE_OK 30s）
    // 在 restart 后跳过探活直打旧 IP；注册表脏 IP 正是本场景要构造的输入
    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    let Some(id_before) = docker_inspect_id(&app) else {
        report.assert_hard(
            "restart 前容器 inspect 可得 Id（基线）",
            false,
            "docker inspect 失败（容器未建或 CLI 异常）".to_string(),
        );
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };

    // docker restart CLI 返回时容器已 Running——无"restart 进行中触发请求走
    // Gone 重建分支"的竞态窗口
    let name = format!("rcoder-app-builder-{app}");
    let restart_ok = std::process::Command::new("docker")
        .args(["restart", &name])
        .output()
        .is_ok_and(|o| o.status.success());

    // 轮询 get-file-list（/api/v1/userapp/* 透传走 resolve_dev_addr 自愈链）：
    // - 探活失败 → remediate → inspect Running → 刷新注册（新 IP）→ 本次转发
    // - 首几次 502 可接受：PROBE_OK 30s 缓存直打旧 IP / file-server ~10s 启动窗
    let mut healed = false;
    let mut last = String::new();
    if restart_ok {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            match env
                .http
                .get(format!(
                    "{}/api/v1/userapp/get-file-list?app_id={app}&user_id={user}",
                    env.rcoder
                ))
                .timeout(Duration::from_secs(30))
                .header("X-App-Id", &app)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let body: Value = resp.json().await.unwrap_or(Value::Null);
                    // get-file-list 是 file-server 直转发的原始形态（success 字段，
                    // 无 HttpResult code 信封——判定对齐场景 1 的两路断言）
                    if status.is_success() && body["success"].as_bool() == Some(true) {
                        healed = true;
                        break;
                    }
                    last = format!("HTTP {status}, {}", trunc(&body, 80));
                }
                Err(e) => last = format!("transport: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    report.assert_hard(
        "restart 后 get-file-list 经自愈恢复 200（探活失败→刷新注册→转发成功）",
        healed,
        format!("restart_ok={restart_ok}, 末次: {last}"),
    );

    // 核心不变量：container_id 不变 = Alive 分支保容器（重建必换 Id）。
    // IP 是否变化不断言——自定 bridge 网络同址复用是合法退化。
    let id_after = docker_inspect_id(&app);
    report.assert_hard(
        "容器 Id 不变（remediate Alive 保容器，非杀重建）",
        matches!(&id_after, Some(a) if *a == id_before),
        format!(
            "before={}…, after={:?}",
            &id_before[..12.min(id_before.len())],
            id_after
        ),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

/// 剥 ANSI 转义序列（ttyd OUTPUT 帧是原始字节含转义，非 base64；
/// 简化 CSI 处理——断言只做 contains，OSC 等罕见形态残留无害）。
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ============================================================
// 场景：userapp 终端 cwd——ws 经 pingora(:8089) → ws_terminal(17681) →
//       ttyd(7681)，wrapper 按注入 cwd 参数起 shell；发 pwd 读回显断言
//       cwd = /home/user/{app}（workspace 压平挂载点）。锚定 f8ccaa8 的
//       cwd.rs UserappBuilder 前缀 env 化修复（终端三方同根）。
//       Docker 模式专属；ttyd 路由脏 IP 不自愈，须用新建容器（不与
//       restart 场景叠加）。
// ============================================================
#[tokio::test]
async fn userapp_dev_terminal_cwd_via_ttyd_ws() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_ttyd";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    if !env.k8s_ssh.is_empty() {
        eprintln!("[{scenario}] K8s 模式跳过（终端 ws 链路 compose 专属场景）");
        return;
    }
    let app = format!("app-{}", scoped_app(&env, "ttyd"));
    let user = "e2e-ud-user";

    // workspace 目录存在即满足 cwd 解析前提（resolve_in_candidates 要求目录在）
    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let pingora = std::env::var("E2E_PINGORA_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8089".to_owned());
    let ws_url = format!("{pingora}/api/v1/userapp/proxy/ttyd/dev/{user}/{app}/ws")
        .replacen("http://", "ws://", 1);

    // 握手重试 30s 窗（ws_terminal 启动等 ttyd 7681 就绪最多 ~15s；子协议 tty
    // 是 ttyd 硬性要求——缺省路由到 http-only 空壳、消息全丢）
    let mut ws = None;
    let handshake_deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < handshake_deadline {
        let Ok(mut req) = ws_url.clone().into_client_request() else {
            break;
        };
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("tty"));
        match connect_async(req).await {
            Ok((stream, _)) => {
                ws = Some(stream);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    let Some(mut ws) = ws else {
        report.assert_hard(
            "终端 ws 握手（pingora → ws_terminal 子协议 tty）",
            false,
            format!("30s 窗口耗尽，url={ws_url}"),
        );
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };
    report.assert_hard(
        "终端 ws 握手（pingora → ws_terminal 子协议 tty）",
        true,
        format!("url={ws_url}"),
    );

    // 首帧 JSON_DATA（ttyd 收到才 fork shell），等 shell 就绪后发 INPUT pwd。
    // 发送失败（连接早断）不阻断——收帧循环自然收不到，由断言统一裁决
    ws.send(Message::Text(r#"{"columns":80,"rows":24}"#.into()))
        .await
        .ok();
    tokio::time::sleep(Duration::from_secs(2)).await;
    ws.send(Message::Binary(b"0pwd\n".to_vec().into()))
        .await
        .ok();

    // 30s 窗收 OUTPUT 帧找 cwd：首字节 0x30 的 Binary 帧，payload 剥 ANSI；
    // 单字节 keepalive(0x90) 帧跳过
    let expected = format!("/home/user/{app}");
    let read_deadline = Instant::now() + Duration::from_secs(30);
    let mut found = false;
    let mut sample = String::new();
    while Instant::now() < read_deadline {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await
        else {
            continue;
        };
        let Message::Binary(data) = msg else {
            continue;
        };
        if data.first() != Some(&b'0') {
            continue;
        }
        let text = strip_ansi(&String::from_utf8_lossy(&data[1..]));
        if text.contains(&expected) {
            found = true;
            break;
        }
        if sample.is_empty() {
            sample = text.chars().take(80).collect();
        }
    }
    report.assert_hard(
        "终端 cwd 落 workspace 压平挂载点（pwd 回显含 /home/user/{app}）",
        found,
        format!("期望含 {expected}, OUTPUT 首帧样本: {sample}"),
    );

    ws.close(None).await.ok();
    assert_hard_all(report).await;
    cleanup_builder(&app);
}

// ============================================================
// P0 三场景：应用代理懒启动 / 受理后高频轮询不误杀 / SSE 游标过头关流
//（锚定 8bb25bd 懒启动、94329c5+Alive 路径探活自愈、SSE 游标关流修复）
// ============================================================

/// P0 场景共享：内存 zip → init-project-template（entries: (zip 内路径, 内容)）。
async fn upload_ws_zip(env: &Env, app: &str, user: &str, entries: &[(&str, &str)]) -> bool {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
    for (path, content) in entries {
        zw.start_file(*path, opts).unwrap();
        std::io::Write::write_all(&mut zw, content.as_bytes()).unwrap();
    }
    let zip_bytes = zw.finish().unwrap().into_inner();
    let part = reqwest::multipart::Part::bytes(zip_bytes).file_name("template.zip");
    let form = reqwest::multipart::Form::new()
        .text("app_id", app.to_owned())
        .text("user_id", user.to_owned())
        .text("enable_git", "false")
        .part("file", part);
    env.http
        .post(format!(
            "{}/api/v1/userapp/init-project-template",
            env.rcoder
        ))
        .timeout(Duration::from_secs(60))
        .header("X-App-Id", app)
        .multipart(form)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// P0 场景共享：dev/start 受理 → task_id（非成功受理返回 None）。
async fn dev_start_task(env: &Env, app: &str, user: &str) -> Option<String> {
    let resp = env
        .http
        .post(format!("{}/api/v1/userapp/dev/start", env.rcoder))
        .timeout(Duration::from_secs(30))
        .header("X-App-Id", app)
        .json(&json!({"app_id": app, "user_id": user}))
        .send()
        .await
        .ok()?;
    let body: Value = resp.json().await.ok()?;
    if !http_ok(&body) {
        return None;
    }
    body["data"]["task_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// P0 场景共享：轮询任务到终态，返回 (status, error)；budget 内未到终态返回 None。
async fn poll_task_terminal(
    env: &Env,
    app: &str,
    user: &str,
    task_id: &str,
    budget: Duration,
    interval: Duration,
) -> Option<(String, String)> {
    let t0 = Instant::now();
    while t0.elapsed() < budget {
        if let Ok(r) = env
            .http
            .get(format!(
                "{}/api/v1/userapp/tasks/{task_id}?app_id={app}&user_id={user}",
                env.rcoder
            ))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            && r.status().is_success()
            && let Ok(b) = r.json::<Value>().await
            && let Some(st) = b["data"]["status"].as_str()
            && matches!(st, "completed" | "failed" | "cancelled")
        {
            return Some((
                st.to_string(),
                b["data"]["error"].as_str().unwrap_or("").to_string(),
            ));
        }
        tokio::time::sleep(interval).await;
    }
    None
}

/// 单服务 workspace 条目（build/run 可定制——P0 三场景共用形态：
/// run = http.server serve 服务目录，readiness 文件 `ready` 使 /ready 200）。
/// 注意：`build_cmd` 内联进 TOML 双引号字符串，不得含双引号/反斜杠
/// （会破坏 manifest 语法——复杂命令改用单引号 TOML 字符串形态）。
fn single_service_entries(build_cmd: &str) -> Vec<(&'static str, String)> {
    let proj_manifest = format!(
        "schema_version = 1\n\n[project]\nservice_id = \"backend-go\"\nname = \"Go Backend\"\ntype = \"go\"\nkind = \"web\"\nenabled = true\n\n[build]\ncommand = [\"sh\", \"-c\", \"{build_cmd}\"]\nartifact = \"artifact.zip\"\n\n[run]\ncommand = [\"sh\", \"-c\", \"touch ready && exec python3 -m http.server $PORT --bind 0.0.0.0\"]\n\n[health]\nreadiness_path = \"/ready\"\n\n[proxy]\npath = \"/api/go/\"\nstrip_prefix = true\n"
    );
    vec![
        (
            "workspace.manifest.toml",
            "schema_version = 1\n\n[workspace]\nname = \"e2e-p0\"\n".to_string(),
        ),
        ("backend-go/project.manifest.toml", proj_manifest),
        ("backend-go/start.sh", "#!/bin/sh\nsleep 9999\n".to_string()),
    ]
}

/// 条目转 `&str` 形态（upload_ws_zip 入参适配）。
fn entries_ref<'a>(entries: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    entries.iter().map(|(p, c)| (*p, c.as_str())).collect()
}

/// 场景：dev 应用代理懒启动——服务跑起后删 builder 容器，应用流量访问
/// 应自动拉起容器（锚定 8bb25bd：app 族代理接 find_dev_container 懒启动，
/// 修复前无容器即 502 永不自愈）。
#[tokio::test]
async fn userapp_dev_app_proxy_lazy_start() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_app_proxy_lazy";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    if !env.k8s_ssh.is_empty() {
        eprintln!("[{scenario}] K8s 模式跳过（docker rm/inspect 专属场景）");
        return;
    }
    let app = format!("app-{}", scoped_app(&env, "lzy"));
    let user = "e2e-ud-user";
    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    let pingora = std::env::var("E2E_PINGORA_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8089".to_owned());
    let svc_url = format!("{pingora}/api/v1/userapp/proxy/app/dev/{user}/{app}/api/go/ready");

    // 1. 起服务（zip build 秒过 + http.server readiness）→ completed
    let entries = single_service_entries("zip -q artifact.zip start.sh");
    report.assert_hard(
        "懒启动前置：init 模板 zip",
        upload_ws_zip(&env, &app, user, &entries_ref(&entries)).await,
        "init-project-template 失败".into(),
    );
    let Some(task_id) = dev_start_task(&env, &app, user).await else {
        report.assert_hard("懒启动前置：dev/start 受理", false, "未拿到 task_id".into());
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };
    let terminal = poll_task_terminal(
        &env,
        &app,
        user,
        &task_id,
        Duration::from_secs(120),
        Duration::from_secs(3),
    )
    .await;
    report.assert_hard(
        "懒启动前置：任务 completed（服务在跑）",
        terminal
            .as_ref()
            .map(|(st, _)| st == "completed")
            .unwrap_or(false),
        format!("terminal={terminal:?}"),
    );

    // 2. 服务路由经代理可达（200）
    let resp = env
        .http
        .get(&svc_url)
        .timeout(Duration::from_secs(15))
        .send()
        .await;
    let svc_status = resp.as_ref().map(|r| r.status().as_u16()).ok();
    report.assert_hard(
        "懒启动前置：代理访问服务路由 200",
        svc_status == Some(200),
        format!("GET {svc_url} -> status={svc_status:?}"),
    );

    // 3. 删容器 → 应用流量访问 → 容器应自动重建（Id 必变）
    let id_before = docker_inspect_id(&app);
    report.assert_hard(
        "懒启动前置：容器 Id 记录",
        id_before.is_some(),
        "docker inspect 未取到容器".into(),
    );
    let Some(id_before) = id_before else {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };
    cleanup_builder(&app);

    let resp = env
        .http
        .get(&svc_url)
        .timeout(Duration::from_secs(60))
        .send()
        .await;
    report.diagnostic(
        "懒启动：删容器后首访状态（新容器服务未编排，5xx 属预期）",
        &format!("{resp:?}"),
        "语义断言在容器重建而非本访状态码",
    );

    // 4. 容器被自动拉回（30s 窗口内出现新 Id 且 ≠ 旧 Id）
    let mut id_after = None;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(30) {
        if let Some(id) = docker_inspect_id(&app) {
            id_after = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    report.assert_hard(
        "懒启动：容器被应用流量自动重建（新 Id ≠ 旧 Id）",
        matches!(&id_after, Some(id) if *id != id_before),
        format!("before={id_before:.12}, after={id_after:?}"),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

/// 场景：受理后高频轮询不误杀容器——dev/start 受理后零等待进入 0.5s 间隔
/// 轮询（精确撞容器启动窗口 + 编译期探活超时窗），容器 Id 应全程唯一、
/// 任务正常达终态（锚定线上 id=80 事故：修复前该节奏触发探活自愈杀容器、
/// 任务蒸发查询 404）。
#[tokio::test]
async fn userapp_dev_poll_storm_keeps_container() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_poll_storm";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    if !env.k8s_ssh.is_empty() {
        eprintln!("[{scenario}] K8s 模式跳过（docker inspect 专属场景）");
        return;
    }
    let app = format!("app-{}", scoped_app(&env, "storm"));
    let user = "e2e-ud-user";
    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 慢 build（~12s 编译窗口）制造探活超时窗
    let entries = single_service_entries("sleep 12 && zip -q artifact.zip start.sh");
    report.assert_hard(
        "轮询风暴前置：init 模板 zip",
        upload_ws_zip(&env, &app, user, &entries_ref(&entries)).await,
        "init-project-template 失败".into(),
    );
    let Some(task_id) = dev_start_task(&env, &app, user).await else {
        report.assert_hard(
            "轮询风暴前置：dev/start 受理",
            false,
            "未拿到 task_id".into(),
        );
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };

    // 零等待高频轮询（0.5s）：启动窗口 + 编译期全程打 tasks 查询
    let mut seen_ids = std::collections::BTreeSet::new();
    let mut query_ok_count = 0u32;
    let mut terminal = None;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(90) {
        if let Some(id) = docker_inspect_id(&app) {
            seen_ids.insert(id);
        }
        if let Ok(r) = env
            .http
            .get(format!(
                "{}/api/v1/userapp/tasks/{task_id}?app_id={app}&user_id={user}",
                env.rcoder
            ))
            .timeout(Duration::from_secs(8))
            .send()
            .await
            && r.status().is_success()
        {
            query_ok_count += 1;
            if let Ok(b) = r.json::<Value>().await
                && let Some(st) = b["data"]["status"].as_str()
                && matches!(st, "completed" | "failed" | "cancelled")
            {
                terminal = Some(st.to_string());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if let Some(id) = docker_inspect_id(&app) {
        seen_ids.insert(id);
    }
    report.diagnostic(
        "轮询风暴：查询成功次数",
        &query_ok_count.to_string(),
        "高频轮询全程打 tasks 查询",
    );
    report.assert_hard(
        "轮询风暴：任务正常达终态（未蒸发）",
        matches!(terminal.as_deref(), Some("completed") | Some("failed")),
        format!("terminal={terminal:?}（90s 未到终态=任务蒸发或卡死）"),
    );
    report.assert_hard(
        "轮询风暴：容器 Id 全程唯一（启动窗口+编译期无重建/误杀）",
        seen_ids.len() == 1,
        format!("seen_ids={seen_ids:?}（>1 即容器曾被杀重建）"),
    );
    report.assert_hard(
        "轮询风暴：容器仍存在",
        docker_inspect_id(&app).is_some(),
        "终态后容器应存活".into(),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}

/// 场景：SSE 游标过头即刻关流——终态任务带越界 from_seq 订阅 logs/stream，
/// 流应数秒内服务端主动关闭且零事件（修复前：replay 空后 broadcast pending，
/// 流只剩 15s keep-alive 永久悬挂、EventSource 无限重连）。
#[tokio::test]
async fn userapp_dev_task_sse_cursor_past_terminal() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    let _gate = scenario_gate().await;
    let scenario = "userapp_dev_sse_cursor";
    let Some((env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let app = format!("app-{}", scoped_app(&env, "cur"));
    let user = "e2e-ud-user";
    if !create_workspace(&env, &report, &app, user).await {
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    }

    // 快速失败任务（build exit 1 → 秒级 failed 终态）
    let entries = single_service_entries("exit 1");
    report.assert_hard(
        "游标关流前置：init 模板 zip",
        upload_ws_zip(&env, &app, user, &entries_ref(&entries)).await,
        "init-project-template 失败".into(),
    );
    let Some(task_id) = dev_start_task(&env, &app, user).await else {
        report.assert_hard(
            "游标关流前置：dev/start 受理",
            false,
            "未拿到 task_id".into(),
        );
        assert_hard_all(report).await;
        cleanup_builder(&app);
        return;
    };
    let terminal = poll_task_terminal(
        &env,
        &app,
        user,
        &task_id,
        Duration::from_secs(60),
        Duration::from_secs(1),
    )
    .await;
    report.assert_hard(
        "游标关流前置：任务达终态",
        terminal.is_some(),
        format!("terminal={terminal:?}"),
    );

    // 带 from_seq=99999（越过终态事件）订阅：10s deadline 内应服务端关流 + 零事件
    let sse_url = format!(
        "{}/api/v1/userapp/tasks/{task_id}/logs/stream?app_id={app}&user_id={user}&from_seq=99999",
        env.rcoder
    );
    let resp = env
        .sse_http
        .get(&sse_url)
        .header("Accept", "text/event-stream")
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    use futures_util::StreamExt;
    let (status, ct, mut stream) = match resp {
        Ok(r) => {
            let status = r.status();
            let ct = r
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            (status, ct, r.bytes_stream())
        }
        Err(e) => {
            report.assert_hard("游标关流：SSE 连接建立", false, format!("{e}"));
            assert_hard_all(report).await;
            cleanup_builder(&app);
            return;
        }
    };
    let mut text = String::new();
    let mut ended_by_close = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                text.push_str(&String::from_utf8_lossy(&chunk));
            }
            Ok(None) => {
                ended_by_close = true; // 服务端主动关流
                break;
            }
            Ok(Some(Err(_))) | Err(_) => break, // 网络错误 / deadline 兜底
        }
    }
    report.assert_hard(
        "游标关流：SSE 200 + event-stream",
        status.is_success() && ct.contains("text/event-stream"),
        format!("HTTP {status}, content-type={ct}"),
    );
    report.assert_hard(
        "游标关流：deadline 内服务端主动关流（非悬挂）",
        ended_by_close,
        format!(
            "ended_by_close={ended_by_close}, body: {}",
            trunc(&Value::String(text.clone()), 120)
        ),
    );
    report.assert_hard(
        "游标关流：越界游标零事件下发",
        !text.contains("event:"),
        format!("body 应为空，实得: {}", trunc(&Value::String(text), 120)),
    );

    assert_hard_all(report).await;
    cleanup_builder(&app);
}
