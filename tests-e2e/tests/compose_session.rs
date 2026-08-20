//! compose 环境会话/重连系场景（gate 同 compose_sse）。
//!
//! 运行: `make test-e2e-compose` 或
//! `cargo test -p rcoder-e2e --test compose_session -- --test-threads=1`
//!
//! 覆盖：游标/无游标重连（双后端）、project→session 映射复用（前端标准
//! 续话姿势）、跨轮重连零残留、三连切模型 + 逐字重放检测、并发订阅。

use std::time::Duration;

use rcoder_e2e::common::scenario::{
    CollectSpec, assert_hard_all, collect_reported, count_event, record_bg_chat, spawn_chat,
};
use rcoder_e2e::common::sse;
use rcoder_e2e::common::{Backend, Env, TestUserGuard, chat_reported};

// ============================================================
// 场景：turn 进行中断开，带 Last-Event-ID 重连——只收增量（双后端）
// ============================================================
async fn scenario_reconnect_cursor(backend: Backend) {
    let scenario = "reconnect_with_cursor";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let user = env.scoped_user(&format!("s4-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let req = env.base_payload(
        backend,
        "写一篇600字左右的散文，主题：山间的清晨。直接正文。",
        &format!("{}-s4", env.run_tag),
        &user,
    );
    let Ok(data) = chat_reported(&env, &report, "turn1", &env.rcoder, &req).await else {
        report.assert_hard("chat 成功", false, "chat 失败（见 chat_request 行）".into());
        assert_hard_all(report).await;
        return;
    };
    let sid = data.session_id;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (evs1, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "first_window",
            entry: &env.rcoder,
            sid: &sid,
            duration_s: 3.0,
            last_event_id: None,
            idle_stop: false,
        },
    )
    .await;
    let ids1 = sse::ids_of(&evs1);
    report.assert_hard(
        "首窗口收到事件",
        !ids1.is_empty(),
        format!("{} 个", ids1.len()),
    );
    let cursor = ids1.iter().copied().max().unwrap_or(0);

    let (evs2, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "reconnect_with_cursor",
            entry: &env.rcoder,
            sid: &sid,
            duration_s: 20.0,
            last_event_id: Some(cursor),
            idle_stop: true,
        },
    )
    .await;
    let ids2 = sse::ids_of(&evs2);
    let min2 = ids2.iter().copied().min();
    report.assert_hard(
        "重连收到增量事件",
        !ids2.is_empty(),
        format!("{} 个；空=turn 已结束或异常", ids2.len()),
    );
    report.assert_hard(
        "增量：全 > 游标",
        min2.is_some_and(|m| m > cursor),
        format!("min={:?} > {cursor}", min2),
    );
    report.assert_hard(
        "重连流 id 单调",
        sse::monotonic_unique(&ids2),
        format!("{} 个 seq", ids2.len()),
    );
    assert_hard_all(report).await;
}

// ============================================================
// 场景：turn 进行中断开，无游标重连——纯实时（零已收消息重放红线，双后端）
// ============================================================
async fn scenario_reconnect_no_cursor(backend: Backend) {
    let scenario = "reconnect_no_cursor";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let user = env.scoped_user(&format!("s5-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let req = env.base_payload(
        backend,
        "写一篇600字左右的散文，主题：海边的黄昏。直接正文。",
        &format!("{}-s5", env.run_tag),
        &user,
    );
    let Ok(data) = chat_reported(&env, &report, "turn1", &env.rcoder, &req).await else {
        report.assert_hard("chat 成功", false, "chat 失败（见 chat_request 行）".into());
        assert_hard_all(report).await;
        return;
    };
    let sid = data.session_id;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (evs1, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "first_window",
            entry: &env.rcoder,
            sid: &sid,
            duration_s: 3.0,
            last_event_id: None,
            idle_stop: false,
        },
    )
    .await;
    let ids1 = sse::ids_of(&evs1);
    let last1 = ids1.iter().copied().max();
    report.assert_hard(
        "首窗口收到事件",
        last1.is_some(),
        format!("{} 个", ids1.len()),
    );
    let Some(last1) = last1 else {
        assert_hard_all(report).await;
        return;
    };

    let (evs2, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "reconnect_no_cursor",
            entry: &env.rcoder,
            sid: &sid,
            duration_s: 20.0,
            last_event_id: None,
            idle_stop: true,
        },
    )
    .await;
    let ids2 = sse::ids_of(&evs2);
    let min2 = ids2.iter().copied().min();
    report.assert_hard(
        "零已收消息重放（红线）",
        min2.is_some_and(|m| m > last1) || ids2.is_empty(),
        format!(
            "重连收到 {} 个（min={:?}，须全 > {last1} 或为空）",
            ids2.len(),
            min2
        ),
    );
    report.assert_hard(
        "重连流 id 单调无重复",
        sse::monotonic_unique(&ids2),
        format!("{} 个 seq", ids2.len()),
    );
    assert_hard_all(report).await;
}

// ============================================================
// 场景：第二轮不带 session_id 只带 project_id——project→session 映射复用
// （前端标准续话姿势；断言 session_id 复用 + SSE 正常 + 上下文延续）
// ============================================================
async fn scenario_no_session_reuse(backend: Backend) {
    let scenario = "no_session_reuse";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let user = env.scoped_user(&format!("s9-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let req1 = env.base_payload(
        backend,
        "请用三点解释 CAP 定理，每点一句话。",
        &format!("{}-s9a", env.run_tag),
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
            duration_s: 40.0,
            last_event_id: None,
            idle_stop: true,
        },
    )
    .await;
    let last1 = sse::ids_of(&evs1).iter().copied().max().unwrap_or(0);

    // 第二轮：只带 user_id + project_id（不带 session_id——前端标准姿势）
    let mut req2 = env.base_payload(
        backend,
        "我上一条消息问了什么？一句话概括，再写一行总结。",
        &format!("{}-s9b", env.run_tag),
        &user,
    );
    req2.project_id = Some(pid.clone());
    debug_assert!(
        req2.session_id.is_none(),
        "second round must NOT carry session_id"
    );
    let handle = spawn_chat(&env, &env.rcoder, req2.clone());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (evs2, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "collect_turn2",
            entry: &env.rcoder,
            sid: &sid,
            duration_s: 60.0,
            last_event_id: None,
            idle_stop: true,
        },
    )
    .await;

    // 等 chat2 返回并留痕（复用判定需要响应里的 session_id）
    let d2 = tokio::time::timeout(Duration::from_secs(140), handle).await;
    match d2 {
        Ok(Ok(Ok(data))) => {
            report.chat_request(rcoder_e2e::common::report::ChatTrace {
                phase: "turn2_bg",
                url: &env.rcoder,
                ok: true,
                request_sanitized: rcoder_e2e::common::sanitize_request(&req2),
                response: Some(&serde_json::to_value(&data).unwrap_or_default()),
                error: None,
                elapsed_ms: 0,
            });
            let head = |v: &str| -> String { v.chars().take(18).collect() };
            report.assert_hard(
                "session_id 复用（响应 == 第一轮）",
                data.session_id == sid,
                format!(
                    "第二轮 {}.. vs 第一轮 {}..",
                    head(&data.session_id),
                    head(&sid)
                ),
            );
        }
        other => {
            let err = match other {
                Ok(Ok(Err(e))) => e.to_string(),
                Ok(Err(j)) => format!("task: {j}"),
                Err(_) => "timeout".to_owned(),
                _ => unreachable!(),
            };
            report.chat_request(rcoder_e2e::common::report::ChatTrace {
                phase: "turn2_bg",
                url: &env.rcoder,
                ok: false,
                request_sanitized: rcoder_e2e::common::sanitize_request(&req2),
                response: None,
                error: Some(&err),
                elapsed_ms: 0,
            });
            report.assert_hard("第二轮 chat 成功", false, err);
        }
    }

    let ids2 = sse::ids_of(&evs2);
    let text2 = sse::chunks_text(&evs2);
    let min2 = ids2.iter().copied().min();
    report.assert_hard(
        "SSE 收到第二轮事件",
        !ids2.is_empty(),
        format!("{} 个", ids2.len()),
    );
    report.assert_hard(
        "seq 延续全 > 第一轮",
        min2.is_some_and(|m| m > last1),
        format!("min={:?} > {last1}", min2),
    );
    report.assert_hard(
        "第二轮完整执行",
        count_event(&evs2, "end_turn") >= 1,
        format!("end_turn {} 个", count_event(&evs2, "end_turn")),
    );
    report.assert_hard(
        "上下文延续（记得 CAP）",
        text2.contains("CAP"),
        format!("回答头 {:?}", text2.chars().take(30).collect::<String>()),
    );
    assert_hard_all(report).await;
}

// ============================================================
// 场景：断线跨越 turn 边界——turn1 终端清后重连收 turn2（零 turn1 残留）
// ============================================================
async fn scenario_cross_turn_reconnect(backend: Backend) {
    let scenario = "cross_turn_reconnect";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let user = env.scoped_user(&format!("sg1-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let req1 = env.base_payload(
        backend,
        "从1数到10，每行一个数字",
        &format!("{}-sg1a", env.run_tag),
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
    let t1_max = ids1.iter().copied().max().unwrap_or(0);
    let t1_text = sse::chunks_text(&evs1);
    report.assert_hard("turn1 收到", !ids1.is_empty(), format!("{} 个", ids1.len()));

    // 等 turn1 结束（终端清）后发 turn2，无游标重连
    tokio::time::sleep(Duration::from_secs(8)).await;
    let mut req2 = env.base_payload(
        backend,
        "写一篇200字短文，主题：雨后的街道。直接正文。",
        &format!("{}-sg1b", env.run_tag),
        &user,
    );
    req2.session_id = Some(sid.clone());
    req2.project_id = Some(pid.clone());
    let handle = spawn_chat(&env, &env.rcoder, req2.clone());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (evs2, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "reconnect_after_terminal",
            entry: &env.rcoder,
            sid: &sid,
            duration_s: 60.0,
            last_event_id: None,
            idle_stop: true,
        },
    )
    .await;
    record_bg_chat(&report, "turn2_bg", &env.rcoder, &req2, handle).await;

    let ids2 = sse::ids_of(&evs2);
    let text2 = sse::chunks_text(&evs2);
    let min2 = ids2.iter().copied().min();
    report.assert_hard(
        "重连收到 turn2",
        !ids2.is_empty(),
        format!("{} 个", ids2.len()),
    );
    report.assert_hard(
        "零 turn1 残留",
        min2.is_some_and(|m| m > t1_max),
        format!("min={:?} > turn1 max={t1_max}", min2),
    );
    report.assert_hard(
        "turn2 完整链（恰 1 prompt_start + ≥1 end_turn）",
        count_event(&evs2, "prompt_start") == 1 && count_event(&evs2, "end_turn") >= 1,
        format!(
            "prompt_start {} 个 / end_turn {} 个",
            count_event(&evs2, "prompt_start"),
            count_event(&evs2, "end_turn")
        ),
    );
    let t1_head: String = t1_text.chars().take(8).collect();
    let t2_head: String = text2.chars().take(40).collect();
    report.assert_hard(
        "turn2 开头非 turn1 内容",
        !t2_head.contains(&t1_head),
        format!("turn2 头 {t2_head:?}（turn1 头 8 字 {t1_head:?}）"),
    );
    assert_hard_all(report).await;
}

// ============================================================
// 场景：三连切模型（默认→pro→默认）+ 每轮逐字重放检测（内容级防重复）
// ============================================================
async fn scenario_model_switch_multi(backend: Backend) {
    let scenario = "model_switch_multi";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    if env.model_pro.is_empty() {
        report.skip("LLM_MODEL_PRO 未配置（.env.local），无切换对象");
        return;
    }
    let user = env.scoped_user(&format!("sm-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let turns: [(&str, &str); 3] = [
        (
            "default",
            "请用三点解释 CAP 定理，每点一句话，最后单独一行总结。",
        ),
        (
            "pro",
            "请用三点解释 BASE 定理，每点一句话，最后单独一行总结。",
        ),
        ("default", "请用两点对比 CAP 与 BASE 的关系，每点一句话。"),
    ];
    let mut sid: Option<String> = None;
    let mut pid: Option<String> = None;
    let mut replies: Vec<String> = Vec::new();
    let mut turn_last_seq: u64 = 0;

    for (i, (tag, prompt)) in turns.iter().enumerate() {
        let model = if *tag == "pro" {
            env.model_pro.as_str()
        } else {
            ""
        };
        let mut req = env.base_payload_with_model(
            backend,
            prompt,
            &format!("{}-sm{}{}", env.run_tag, i, backend.as_str()),
            &user,
            model,
        );
        if let (Some(s), Some(p)) = (&sid, &pid) {
            req.session_id = Some(s.clone());
            req.project_id = Some(p.clone());
        }

        let evs = if sid.is_none() {
            // 首轮同步（拿 sid/pid 后立即收 SSE，turn 尚在进行）
            match chat_reported(&env, &report, &format!("turn{i}"), &env.rcoder, &req).await {
                Ok(d) => {
                    sid = Some(d.session_id.clone());
                    pid = Some(d.project_id.clone());
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    let (evs, _) = collect_reported(
                        &env,
                        &report,
                        CollectSpec {
                            phase: &format!("collect_turn{i}"),
                            entry: &env.rcoder,
                            sid: &d.session_id,
                            duration_s: 60.0,
                            last_event_id: None,
                            idle_stop: true,
                        },
                    )
                    .await;
                    evs
                }
                Err(_) => {
                    report.assert_hard(
                        &format!("turn{i}({tag}) chat 成功"),
                        false,
                        "chat 失败（见 chat_request 行）".into(),
                    );
                    assert_hard_all(report).await;
                    return;
                }
            }
        } else {
            let handle = spawn_chat(&env, &env.rcoder, req.clone());
            tokio::time::sleep(Duration::from_secs(1)).await;
            let s = sid.clone().unwrap_or_default();
            let (evs, _) = collect_reported(
                &env,
                &report,
                CollectSpec {
                    phase: &format!("collect_turn{i}"),
                    entry: &env.rcoder,
                    sid: &s,
                    duration_s: 60.0,
                    last_event_id: None,
                    idle_stop: true,
                },
            )
            .await;
            record_bg_chat(&report, &format!("turn{i}_bg"), &env.rcoder, &req, handle).await;
            evs
        };

        let ids = sse::ids_of(&evs);
        let text = sse::chunks_text(&evs);
        let min_id = ids.iter().copied().min();
        report.assert_hard(
            &format!("turn{i}({tag}) 收到事件"),
            !ids.is_empty(),
            format!("{} 个", ids.len()),
        );
        report.assert_hard(
            &format!("turn{i}({tag}) seq 全 > 前轮（隔离）"),
            min_id.is_some_and(|m| m > turn_last_seq),
            format!("min={:?} > {turn_last_seq}", min_id),
        );
        report.assert_hard(
            &format!("turn{i}({tag}) 完整执行"),
            count_event(&evs, "end_turn") >= 1,
            format!("end_turn {} 个", count_event(&evs, "end_turn")),
        );
        turn_last_seq = ids.iter().copied().max().unwrap_or(turn_last_seq);
        // 逐字重放检测：本轮流不包含前面任何轮回答的 24 字连续片段
        for (j, prev) in replies.iter().enumerate() {
            let frag = sse::longest_common_snippet(prev, &text, 24);
            report.assert_hard(
                &format!("turn{i} 无 turn{j} 逐字重放"),
                frag.is_none(),
                match &frag {
                    Some(f) => format!("命中片段 {f:?}"),
                    None => "24 字窗口无命中".into(),
                },
            );
        }
        replies.push(text);
    }

    // 末轮上下文：应记得前两轮话题（CAP 与 BASE 对比）
    let last = replies.last().cloned().unwrap_or_default();
    report.assert_hard(
        "末轮上下文延续（对比 CAP 与 BASE）",
        last.contains("CAP") && last.contains("BASE"),
        format!("末轮回答头 {:?}", last.chars().take(40).collect::<String>()),
    );
    assert_hard_all(report).await;
}

// ============================================================
// 场景：多客户端并发订阅同一 session——两路都完整收到、seq 交集（双后端）
// ============================================================
async fn scenario_concurrent_subscribers(backend: Backend) {
    let scenario = "concurrent_subscribers";
    let Some((env, report)) = Env::compose_or_skip(scenario, backend.as_str()).await else {
        return;
    };
    let user = env.scoped_user(&format!("sg2-{}", backend.as_str()));
    let _guard = TestUserGuard::new(&env, &user);

    let req = env.base_payload(
        backend,
        "从1数到8，每行一个数字",
        &format!("{}-sg2", env.run_tag),
        &user,
    );
    let Ok(data) = chat_reported(&env, &report, "turn1", &env.rcoder, &req).await else {
        report.assert_hard("chat 成功", false, "chat 失败（见 chat_request 行）".into());
        assert_hard_all(report).await;
        return;
    };
    let sid = data.session_id;

    tokio::time::sleep(Duration::from_millis(600)).await;
    // 两路并发订阅（jsonl 的 sse_event 行按 phase=A/B 区分）
    let ((evs_a, _), (evs_b, _)) = tokio::join!(
        collect_reported(
            &env,
            &report,
            CollectSpec {
                phase: "subscriber_A",
                entry: &env.rcoder,
                sid: &sid,
                duration_s: 30.0,
                last_event_id: None,
                idle_stop: true,
            }
        ),
        collect_reported(
            &env,
            &report,
            CollectSpec {
                phase: "subscriber_B",
                entry: &env.rcoder,
                sid: &sid,
                duration_s: 30.0,
                last_event_id: None,
                idle_stop: true,
            }
        ),
    );
    let ids_a: std::collections::HashSet<u64> = sse::ids_of(&evs_a).into_iter().collect();
    let ids_b: std::collections::HashSet<u64> = sse::ids_of(&evs_b).into_iter().collect();
    let inter = ids_a.intersection(&ids_b).count();

    report.assert_hard(
        "两个订阅者都收到",
        !ids_a.is_empty() && !ids_b.is_empty(),
        format!("A={}，B={}", ids_a.len(), ids_b.len()),
    );
    let end_any = count_event(&evs_a, "end_turn") > 0 || count_event(&evs_b, "end_turn") > 0;
    report.assert_hard(
        "至少一端收到完整轮（end_turn）",
        end_any,
        "首连资格语义：其一获得 replay".into(),
    );
    report.assert_hard(
        "实时流共享（seq 交集）",
        inter > 0,
        format!("交集 {inter} 个"),
    );
    let (ta, tb) = (sse::chunks_text(&evs_a), sse::chunks_text(&evs_b));
    report.assert_hard(
        "两者内容非空",
        !ta.is_empty() && !tb.is_empty(),
        format!("A={} 字，B={} 字", ta.chars().count(), tb.chars().count()),
    );
    assert_hard_all(report).await;
}

// ============================================================
// 测试入口
// ============================================================

#[tokio::test]
async fn reconnect_with_cursor_openai() {
    scenario_reconnect_cursor(Backend::Openai).await;
}

#[tokio::test]
async fn reconnect_with_cursor_anthropic() {
    scenario_reconnect_cursor(Backend::Anthropic).await;
}

#[tokio::test]
async fn reconnect_no_cursor_openai() {
    scenario_reconnect_no_cursor(Backend::Openai).await;
}

#[tokio::test]
async fn reconnect_no_cursor_anthropic() {
    scenario_reconnect_no_cursor(Backend::Anthropic).await;
}

#[tokio::test]
async fn no_session_reuse_openai() {
    scenario_no_session_reuse(Backend::Openai).await;
}

#[tokio::test]
async fn no_session_reuse_anthropic() {
    scenario_no_session_reuse(Backend::Anthropic).await;
}

#[tokio::test]
async fn cross_turn_reconnect_openai() {
    scenario_cross_turn_reconnect(Backend::Openai).await;
}

#[tokio::test]
async fn cross_turn_reconnect_anthropic() {
    scenario_cross_turn_reconnect(Backend::Anthropic).await;
}

#[tokio::test]
async fn model_switch_multi_openai() {
    scenario_model_switch_multi(Backend::Openai).await;
}

#[tokio::test]
async fn model_switch_multi_anthropic() {
    scenario_model_switch_multi(Backend::Anthropic).await;
}

#[tokio::test]
async fn concurrent_subscribers_openai() {
    scenario_concurrent_subscribers(Backend::Openai).await;
}

#[tokio::test]
async fn concurrent_subscribers_anthropic() {
    scenario_concurrent_subscribers(Backend::Anthropic).await;
}
