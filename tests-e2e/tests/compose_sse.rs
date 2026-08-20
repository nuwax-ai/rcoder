//! compose 环境 SSE 核心场景（gate: RCODER_URL /health 可达 + LLM 配置完整）。
//!
//! 运行: `make test-e2e-compose` 或
//! `cargo test -p rcoder-e2e --test compose_sse -- --test-threads=1`
//!
//! 场景语义与 Python 套件（tests/sse_e2e/scenarios_*.py）逐一对齐；
//! 断言明细与事件流留痕见 tests-e2e/reports/<run>/<scenario>__<backend>.jsonl。

use std::time::Duration;

use rcoder_e2e::common::scenario::{
    CollectSpec, assert_hard_all, collect_reported, count_event, record_bg_chat, spawn_chat,
};
use rcoder_e2e::common::sse;
use rcoder_e2e::common::{Backend, Env, TestUserGuard, chat_reported};

// ============================================================
// 场景 1：chat 后立刻连 SSE——完整轮 + id 单调（双后端）
// ============================================================
async fn scenario_full_turn(backend: Backend) {
    let scenario = "full_turn_delivery";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let user = env.scoped_user(&format!("s1-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let req = env.base_payload(
        backend,
        "从1数到6，每行一个数字",
        &format!("{}-s1", env.run_tag),
        &user,
    );
    let Ok(data) = chat_reported(&env, &report, "turn1", &env.rcoder, &req).await else {
        report.assert_hard("chat 成功", false, "chat 失败（见 chat_request 行）".into());
        assert_hard_all(report).await;
        return;
    };
    let sid = data.session_id;
    if !report.assert_hard("session_id 非空", !sid.is_empty(), sid.clone()) {
        assert_hard_all(report).await;
        return;
    }

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
    report.assert_hard("id 行存在", !ids.is_empty(), format!("{} 个", ids.len()));
    report.assert_hard(
        "id 单调无重复",
        sse::monotonic_unique(&ids),
        format!("{} 个 seq", ids.len()),
    );
    let head: String = text.chars().take(30).collect();
    report.assert_hard(
        "回答含数字内容",
        text.chars().any(|ch| ch.is_ascii_digit()),
        format!("回答头部 {head:?}"),
    );
    report.diagnostic("回答文本", &text, "agent_message_chunk 拼接全文");

    assert_hard_all(report).await;
}

// ============================================================
// 场景 2：turn 结束后连 SSE——0 消息事件（终端即清）
// ============================================================
async fn scenario_after_terminal(backend: Backend) {
    let scenario = "after_terminal_empty";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let user = env.scoped_user(&format!("s2-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let req = env.base_payload(
        backend,
        "回答一个字：好",
        &format!("{}-s2", env.run_tag),
        &user,
    );
    let Ok(data) = chat_reported(&env, &report, "turn1", &env.rcoder, &req).await else {
        report.assert_hard("chat 成功", false, "chat 失败（见 chat_request 行）".into());
        assert_hard_all(report).await;
        return;
    };
    // 等 turn 完成（终端即清已执行）
    tokio::time::sleep(Duration::from_secs(12)).await;
    let (events, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "collect_after_terminal",
            entry: &env.rcoder,
            sid: &data.session_id,
            duration_s: 6.0,
            last_event_id: None,
            idle_stop: true,
        },
    )
    .await;
    let msgs = sse::message_events(&events);
    let msg_types: Vec<&str> = msgs.iter().map(|e| e.event.as_str()).collect();
    report.assert_hard(
        "0 消息事件（终端即清）",
        msgs.is_empty(),
        format!("实际 {} 条：{msg_types:?}；元事件不计入", msgs.len()),
    );
    assert_hard_all(report).await;
}

// ============================================================
// 场景 3：第二轮流不含第一轮（seq 隔离）
// ============================================================
async fn scenario_two_turn_isolation(backend: Backend) {
    let scenario = "two_turn_isolation";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let user = env.scoped_user(&format!("s3-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let req1 = env.base_payload(
        backend,
        "从1数到4，每行一个数字",
        &format!("{}-s3a", env.run_tag),
        &user,
    );
    let Ok(d1) = chat_reported(&env, &report, "turn1", &env.rcoder, &req1).await else {
        report.assert_hard("chat 成功", false, "chat 失败（见 chat_request 行）".into());
        assert_hard_all(report).await;
        return;
    };
    let (sid, pid) = (d1.session_id.clone(), d1.project_id.clone());
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
    let ids1 = sse::ids_of(&evs1);
    let last1 = ids1.iter().copied().max().unwrap_or(0);

    // 第二轮：同 session 后台发（任务要够长，短任务会在连接前终结清空）
    // 续话必须同时带 session_id + project_id（acp-ts 的 session 存储按 cwd）
    let mut req2 = env.base_payload(
        backend,
        "写一篇300字左右的短文，主题：城市的夜晚。直接正文。",
        &format!("{}-s3b", env.run_tag),
        &user,
    );
    req2.session_id = Some(sid.clone());
    req2.project_id = Some(pid.clone());
    let handle = spawn_chat(&env, &env.rcoder, req2.clone());
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
    record_bg_chat(&report, "turn2_bg", &env.rcoder, &req2, handle).await;

    let ids2 = sse::ids_of(&evs2);
    let min2 = ids2.iter().copied().min();
    report.assert_hard(
        "第二轮 seq 全 > 第一轮最大",
        min2.is_some_and(|m| m > last1),
        format!("min={:?} > {last1}", min2),
    );
    report.assert_hard(
        "恰一个 prompt_start",
        count_event(&evs2, "prompt_start") == 1,
        format!("{} 个", count_event(&evs2, "prompt_start")),
    );
    report.assert_hard(
        "恰一个 end_turn",
        count_event(&evs2, "end_turn") == 1,
        format!("{} 个", count_event(&evs2, "end_turn")),
    );
    report.diagnostic("turn1 文本", &sse::chunks_text(&evs1), "第一轮拼接全文");
    report.diagnostic("turn2 文本", &sse::chunks_text(&evs2), "第二轮拼接全文");

    assert_hard_all(report).await;
}

// ============================================================
// 场景 6：同 session+project 切模型（flash→pro）——零重放 + 上下文延续
// ============================================================
async fn scenario_model_switch(backend: Backend) {
    let scenario = "model_switch";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    if env.model_pro.is_empty() {
        report.skip("LLM_MODEL_PRO 未配置（.env.local），无切换对象");
        return;
    }
    let user = env.scoped_user(&format!("s6-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let req1 = env.base_payload(
        backend,
        "用三点解释 CAP 定理，每点一句话，最后一行总结。",
        &format!("{}-s6a", env.run_tag),
        &user,
    );
    let Ok(d1) = chat_reported(&env, &report, "turn1", &env.rcoder, &req1).await else {
        report.assert_hard("chat 成功", false, "chat 失败（见 chat_request 行）".into());
        assert_hard_all(report).await;
        return;
    };
    let (sid, pid) = (d1.session_id.clone(), d1.project_id.clone());
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
    let ids1 = sse::ids_of(&evs1);
    let last1 = ids1.iter().copied().max().unwrap_or(0);

    // 第二轮：切 pro 模型（id/name/default_model 三字段），后台发 chat
    let mut req2 = env.base_payload(
        backend,
        "我上一条问了什么？一句话概括，再三点解释 BASE 定理。",
        &format!("{}-s6b", env.run_tag),
        &user,
    );
    req2.session_id = Some(sid.clone());
    req2.project_id = Some(pid.clone());
    if let Some(mp) = req2.model_provider.as_mut() {
        mp.id = env.model_pro.clone();
        mp.name = env.model_pro.clone();
        mp.default_model = env.model_pro.clone();
    }
    let handle = spawn_chat(&env, &env.rcoder, req2.clone());
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
    record_bg_chat(&report, "turn2_bg", &env.rcoder, &req2, handle).await;

    let ids2 = sse::ids_of(&evs2);
    let min2 = ids2.iter().copied().min();
    let has_err = count_event(&evs2, "error") > 0;
    report.assert_hard(
        "第二轮收到事件",
        !ids2.is_empty(),
        format!("{} 个", ids2.len()),
    );
    report.assert_hard(
        "第二轮 seq 全 > 第一轮（零历史重放）",
        min2.is_some_and(|m| m > last1),
        format!("min={:?} > {last1}", min2),
    );
    if has_err {
        report.diagnostic(
            "切模型 error 事件",
            "出现",
            "已知问题：ProviderModelNotFoundError（见 sse_event error 行）",
        );
    } else {
        report.diagnostic("切模型 error 事件", "无", "模型切换正常");
    }
    // acp-ts 后端模型来自 env（每次进程读取），无 opencode 的 session 引用
    // 持久化问题——切模型应完整成功且上下文延续（对齐 Python s8 场景）
    if backend == Backend::Anthropic {
        let text2 = sse::chunks_text(&evs2);
        report.assert_hard(
            "上下文延续（记得 CAP）+ 回答了新问题 BASE",
            text2.contains("CAP") && (text2.contains("BASE") || text2.contains("基本可用")),
            format!("回答头 {:?}", text2.chars().take(30).collect::<String>()),
        );
    }
    report.diagnostic("turn1 文本", &sse::chunks_text(&evs1), "第一轮拼接全文");
    report.diagnostic(
        "turn2 文本",
        &sse::chunks_text(&evs2),
        "第二轮拼接全文（应含 CAP 概括=上下文延续）",
    );
    assert_hard_all(report).await;
}

// ============================================================
// 测试入口（cargo test 名称 = 场景名__后端）
// ============================================================

#[tokio::test]
async fn full_turn_openai() {
    scenario_full_turn(Backend::Openai).await;
}

#[tokio::test]
async fn full_turn_anthropic() {
    scenario_full_turn(Backend::Anthropic).await;
}

#[tokio::test]
async fn after_terminal_openai() {
    scenario_after_terminal(Backend::Openai).await;
}

#[tokio::test]
async fn after_terminal_anthropic() {
    scenario_after_terminal(Backend::Anthropic).await;
}

#[tokio::test]
async fn two_turn_isolation_openai() {
    scenario_two_turn_isolation(Backend::Openai).await;
}

#[tokio::test]
async fn two_turn_isolation_anthropic() {
    scenario_two_turn_isolation(Backend::Anthropic).await;
}

#[tokio::test]
async fn model_switch_openai() {
    scenario_model_switch(Backend::Openai).await;
}

#[tokio::test]
async fn model_switch_anthropic() {
    scenario_model_switch(Backend::Anthropic).await;
}

// gate 冒烟：不产生 chat 流量，验证 skip 路径可走通。
#[tokio::test]
async fn gate_compose_or_skip_smoke() {
    if let Some((_env, report)) = Env::compose_or_skip("gate_smoke", "openai").await {
        report.skip("smoke: gate passed, no scenario body");
    }
}
