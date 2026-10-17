//! K8s 专项测试（lb_test.py 完整移植）：同一会话经不同节点入口 NodePort /
//! 不同 rcoder 副本，验证会话延续、seq 连续、无重复/丢失。
//!
//! 配置全部经 .env.local / 环境变量（见 .env.local.example），代码零 IP 硬编码：
//! - `TEST_K8S_SSH=user@host`（解析与清理，ssh 免密）
//! - `LB_ENTRY_HOSTS=节点IP列表`（逗号分隔；单节点传一个即可，入口轮换
//!   退化为同入口，SSE 语义场景仍有效——多入口负载均衡语义需多节点集群）
//! - `LB_NODEPORT`（默认 30295）、`TEST_K8S_NS`（默认 nuwax-k8s-test）
//!
//! ⚠️ 测试目标机器：个人开发测试 K8s（20 / 229，单节点）；**19 机有生产
//! 环境，未经明确指示不得对其跑本测试**。
//!
//! **负载均衡场景默认 ignore**（此前已验证通过，重跑须明确决定）：
//! ```bash
//! cargo test -p rcoder-e2e --test k8s_lb -- --ignored --test-threads=1
//! ```

use std::time::Duration;

use rcoder_e2e::common::report::JsonlReporter;
use rcoder_e2e::common::scenario::{
    CollectSpec, assert_hard_all, collect_reported, record_bg_chat, spawn_chat,
};
use rcoder_e2e::common::sse;
use rcoder_e2e::common::{Backend, Env, TestUserGuard, chat_reported};
use serde_json::json;

/// K8s 节点入口列表（NodePort，配置见 Env.lb_entry_hosts；主机名或 IP 均可）。
/// 多入口时 kube-proxy 随机落点，多轮轮换自然遍历 "chat 落副本 X、SSE 落
/// 副本 Y" 的组合；单入口退化为同入口（SSE 语义断言仍有效）。
fn k8s_entries(env: &Env) -> Vec<String> {
    env.lb_entry_hosts
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| format!("http://{h}:{}", env.lb_nodeport))
        .collect()
}

/// K8s gate：TEST_K8S_SSH 存在 + 首入口 /health 可达。
async fn k8s_or_skip(scenario: &str) -> Option<(Env, JsonlReporter, Vec<String>)> {
    if !rcoder_e2e::common::require_context_or_skip() {
        return None;
    }
    let env = Env::load();
    // entries 提前构造（环境行记录实际入口，排查时可见）
    let entries = k8s_entries(&env);
    let report = JsonlReporter::begin(
        scenario,
        "k8s",
        json!({
            // rcoder 字段记实际入口（首入口；metrics diff 拉远端而非本地默认）。
            // entries 可能为空（LB_ENTRY_HOSTS 未配置），此时先记空串、走下方 gate skip
            "rcoder": entries.first().cloned().unwrap_or_default(), "model": env.model, "user": env.user,
            "k8s_ssh": env.k8s_ssh, "k8s_ns": env.k8s_ns,
            "entries": entries, "trace_id": env.trace_id,
        }),
    );
    if env.k8s_ns == "nuwax-k8s-prod" {
        report.skip("production cluster is outside E2E scope");
        return None;
    }
    if env.k8s_ssh.is_empty() {
        report.skip("K8s gate: TEST_K8S_SSH 未设置");
        return None;
    }
    if env.api_key.is_empty() || env.model.is_empty() || env.base_url.is_empty() {
        report.skip("LLM config missing（.env.local / LLM_* 配置）");
        return None;
    }
    if entries.is_empty() {
        report.skip("LB_ENTRY_HOSTS 未配置或解析为空（.env.local / 环境变量）");
        return None;
    }
    match env
        .http
        .get(format!("{}/health", entries[0]))
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => Some((env, report, entries)),
        Ok(r) => {
            report.skip(&format!(
                "K8s gate: 入口 {} /health HTTP {}",
                entries[0],
                r.status()
            ));
            None
        }
        Err(e) => {
            report.skip(&format!("K8s gate: 入口 {} 不可达: {e}", entries[0]));
            None
        }
    }
}

/// 场景 1（主路径）：chat 三节点 NodePort 轮换 + SSE 入口轮换。
/// 真实用户路径：宿主机 IP + NodePort；多轮轮换遍历跨副本组合。
async fn scenario_entry_rotation() {
    let scenario = "lb_entry_rotation";
    let Some((env, report, entries)) = k8s_or_skip(scenario).await else {
        return;
    };
    let user = env.scoped_user("lb");
    let _guard = TestUserGuard::new(&env, &user);

    let prompts = [
        "从1数到5，每行一个数字",
        "我上一条让你做什么了？一句话回答，再解释 CAP 定理三点",
        "再解释 BASE 定理三点",
        "最后对比 CAP 和 BASE 的关系，两点",
    ];
    let mut replies: Vec<String> = Vec::new();
    let mut turn_max: u64 = 0;
    let mut sid: Option<String> = None;
    let mut pid: Option<String> = None;

    for (i, prompt) in prompts.iter().enumerate() {
        let chat_url = entries[i % entries.len()].clone();
        let sse_url = entries[(i + 1) % entries.len()].clone();
        let mut req = env.base_payload(
            Backend::Openai,
            prompt,
            &format!("{}-lb{i}", env.run_tag),
            &user,
        );
        if let (Some(s), Some(p)) = (&sid, &pid) {
            req.session_id = Some(s.clone());
            req.project_id = Some(p.clone());
        }
        let phase = format!("turn{i}");

        if sid.is_none() {
            // 首轮同步 chat 拿 sid
            match chat_reported(&env, &report, &phase, &chat_url, &req).await {
                Ok(d) => {
                    sid = Some(d.session_id.clone());
                    pid = Some(d.project_id.clone());
                }
                Err(_) => {
                    report.assert_hard(
                        &format!("turn{i} chat 成功"),
                        false,
                        format!("chat@{chat_url} 失败（见 chat_request 行）"),
                    );
                    assert_hard_all(report).await;
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        } else {
            // 后续轮后台 chat（同步等返回会错过 SSE 窗口）
            let handle = spawn_chat(&env, &chat_url, req.clone());
            tokio::time::sleep(Duration::from_secs(1)).await;
            let s = sid.clone().unwrap_or_default();
            let (evs, _) = collect_reported(
                &env,
                &report,
                CollectSpec {
                    phase: &format!("collect_{phase}"),
                    entry: &sse_url,
                    sid: &s,
                    duration_s: 45.0,
                    last_event_id: None,
                    idle_stop: true,
                },
            )
            .await;
            record_bg_chat(&report, &format!("{phase}_bg"), &chat_url, &req, handle).await;
            finish_rotation_turn(
                &report,
                i,
                &chat_url,
                &sse_url,
                evs,
                &mut replies,
                &mut turn_max,
            );
            continue;
        }
        let s = sid.clone().unwrap_or_default();
        let (evs, _) = collect_reported(
            &env,
            &report,
            CollectSpec {
                phase: &format!("collect_{phase}"),
                entry: &chat_url,
                sid: &s,
                duration_s: 45.0,
                last_event_id: None,
                idle_stop: true,
            },
        )
        .await;
        finish_rotation_turn(
            &report,
            i,
            &chat_url,
            &chat_url,
            evs,
            &mut replies,
            &mut turn_max,
        );
    }

    // 跨入口上下文延续（第 2/3/4 轮应提及 CAP/BASE）
    let ctx_ok = replies.len() >= 4
        && replies[1].contains("CAP")
        && replies[2].contains("BASE")
        && replies[3].contains("CAP");
    report.assert_hard("跨入口上下文延续（CAP/BASE 关键词）", ctx_ok, {
        let last: String = replies
            .last()
            .map(|r| r.chars().take(40).collect())
            .unwrap_or_default();
        format!("末轮回答头 {last:?}")
    });
    assert_hard_all(report).await;
}

/// 轮次断言（收到事件 / seq 跨入口单源连续 / 完整执行 / 无前轮逐字重放）。
fn finish_rotation_turn(
    report: &JsonlReporter,
    i: usize,
    chat_url: &str,
    sse_url: &str,
    evs: Vec<sse::SseEvent>,
    replies: &mut Vec<String>,
    turn_max: &mut u64,
) {
    let ids = sse::ids_of(&evs);
    let text = sse::chunks_text(&evs);
    let min_id = ids.iter().copied().min();
    report.assert_hard(
        &format!("turn{i} 跨入口收到事件"),
        !ids.is_empty(),
        format!("chat@{chat_url} SSE@{sse_url}：{} 个", ids.len()),
    );
    report.assert_hard(
        &format!("turn{i} seq 全 > 前轮（跨入口 seq 单源连续）"),
        min_id.is_some_and(|m| m > *turn_max),
        format!("min={:?} > {turn_max}", min_id),
    );
    report.assert_hard(
        &format!("turn{i} 完整执行（end_turn）"),
        evs.iter().any(|e| e.event == "end_turn"),
        "完整轮".into(),
    );
    *turn_max = ids.iter().copied().max().unwrap_or(*turn_max);
    // 无前轮逐字重放（内容级重放检测）
    for (j, prev) in replies.iter().enumerate() {
        if let Some(frag) = sse::longest_common_snippet(prev, &text, 24) {
            report.assert_hard(
                &format!("turn{i} 无 turn{j} 逐字重放"),
                false,
                format!("命中片段 {frag:?}"),
            );
        } else {
            report.assert_hard(
                &format!("turn{i} 无 turn{j} 逐字重放"),
                true,
                "24 字窗口无命中".into(),
            );
        }
    }
    replies.push(text);
}

/// 场景 2：SSE 断于入口 A，带游标续于入口 B（真实用户断线路径）。
async fn scenario_cross_entry_cursor_reconnect() {
    let scenario = "lb_cross_entry_cursor_reconnect";
    let Some((env, report, entries)) = k8s_or_skip(scenario).await else {
        return;
    };
    let user = env.scoped_user("lr");
    let _guard = TestUserGuard::new(&env, &user);
    // 循环索引防越界：单入口（LB_ENTRY_HOSTS 只配一个）时 A/B/C 退化为同入口
    let entry_at = |i: usize| entries[i % entries.len()].clone();
    let (a, b, c) = (entry_at(0), entry_at(1), entries[entries.len() - 1].clone());

    // 首轮 chat@A 拿 sid
    let req1 = env.base_payload(
        Backend::Openai,
        "从10数到15，每行一个",
        &format!("{}-lr", env.run_tag),
        &user,
    );
    let Ok(d1) = chat_reported(&env, &report, "turn1", &a, &req1).await else {
        report.assert_hard(
            "chat 成功",
            false,
            "chat@A 失败（见 chat_request 行）".into(),
        );
        assert_hard_all(report).await;
        return;
    };
    let sid = d1.session_id;

    // 二轮后台 chat@B
    let mut req2 = env.base_payload(
        Backend::Openai,
        "我数到几了？直接答数字",
        &format!("{}-lr2", env.run_tag),
        &user,
    );
    req2.session_id = Some(sid.clone());
    req2.project_id = Some(d1.project_id.clone());
    let handle = spawn_chat(&env, &b, req2.clone());
    tokio::time::sleep(Duration::from_millis(800)).await;

    // A 收一段即断
    let (evs_a, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "segment_at_A",
            entry: &a,
            sid: &sid,
            duration_s: 12.0,
            last_event_id: None,
            idle_stop: false,
        },
    )
    .await;
    let ids_a = sse::ids_of(&evs_a);
    report.assert_hard(
        "首段经入口 A 收到事件",
        !ids_a.is_empty(),
        format!("{} 个", ids_a.len()),
    );
    let cursor = ids_a.iter().copied().max().unwrap_or(0);

    // B 带游标续
    let (evs_b, _) = collect_reported(
        &env,
        &report,
        CollectSpec {
            phase: "resume_at_C_with_cursor",
            entry: &c,
            sid: &sid,
            duration_s: 40.0,
            last_event_id: Some(cursor),
            idle_stop: true,
        },
    )
    .await;
    record_bg_chat(&report, "turn2_bg", &b, &req2, handle).await;
    let ids_b = sse::ids_of(&evs_b);
    let min_b = ids_b.iter().copied().min();
    report.assert_hard(
        "续传事件全 > 游标（无重复）",
        min_b.is_some_and(|m| m > cursor) || ids_b.is_empty(),
        format!("min={:?} > {cursor}（空=turn 已结束零增量，正确）", min_b),
    );
    report.assert_hard(
        "跨入口续传符合轮次边界",
        evs_a.iter().any(|e| e.event == "end_turn") || !ids_b.is_empty(),
        format!(
            "first segment completed={}, resumed events={}",
            evs_a.iter().any(|e| e.event == "end_turn"),
            ids_b.len()
        ),
    );
    // 首段窗口内 turn 已结束 → 终端即清后无增量是正确行为；未结束 → 必须收到后续事件
    let first_seg_done = evs_a.iter().any(|e| e.event == "end_turn");
    if first_seg_done {
        report.assert_hard(
            "turn 已结束：续传零增量且无重复（正确）",
            true,
            format!("续传 {} 个", ids_b.len()),
        );
    } else {
        report.assert_hard(
            "turn 进行中：续传收到后续事件（不丢）",
            !ids_b.is_empty(),
            format!("续传 {} 个", ids_b.len()),
        );
    }
    assert_hard_all(report).await;
}

/// 场景 3：新会话 chat 后 1s 从另一入口订阅（durable 直写 + SSE 回源验收）。
/// NodePort 随机落点，多轮组合覆盖跨副本（3 副本下每轮 2/3 概率 X≠Y）。
async fn scenario_new_session_cross_entry() {
    let scenario = "lb_new_session_cross_entry";
    let Some((env, report, entries)) = k8s_or_skip(scenario).await else {
        return;
    };
    let user = env.scoped_user("ln");
    let _guard = TestUserGuard::new(&env, &user);

    for (i, kw) in ["分布式", "微服务", "负载均衡"].iter().enumerate() {
        let chat_url = entries[i % entries.len()].clone();
        let sse_url = entries[(i + 1) % entries.len()].clone();

        // 首轮 chat（新会话）
        let req1 = env.base_payload(
            Backend::Openai,
            &format!("用一句话解释：{kw}。回答必须原样包含“{kw}”这个词。"),
            &format!("{}-n{i}", env.run_tag),
            &user,
        );
        let Ok(d) = chat_reported(&env, &report, &format!("turn{i}"), &chat_url, &req1).await
        else {
            report.assert_hard(
                &format!("轮{i} chat 成功"),
                false,
                format!("chat@{chat_url} 失败"),
            );
            assert_hard_all(report).await;
            return;
        };
        let sid = d.session_id.clone();

        // 二轮补充（后台，同 session）
        let mut req2 = env.base_payload(
            Backend::Openai,
            &format!("再补充一句关于{kw}的要点，回答必须原样包含“{kw}”这个词。"),
            &format!("{}-n{i}b", env.run_tag),
            &user,
        );
        req2.session_id = Some(sid.clone());
        req2.project_id = Some(d.project_id.clone());
        let handle = spawn_chat(&env, &chat_url, req2.clone());

        // 验收窗口：durable+回源后必须直接命中（1s）
        tokio::time::sleep(Duration::from_secs(1)).await;
        let (evs, _) = collect_reported(
            &env,
            &report,
            CollectSpec {
                phase: &format!("collect_{i}_cross_entry"),
                entry: &sse_url,
                sid: &sid,
                duration_s: 40.0,
                last_event_id: None,
                idle_stop: true,
            },
        )
        .await;
        record_bg_chat(&report, &format!("turn{i}b_bg"), &chat_url, &req2, handle).await;

        let ids = sse::ids_of(&evs);
        let text = sse::chunks_text(&evs);
        report.assert_hard(
            &format!("轮{i} 新会话跨入口收到事件"),
            !ids.is_empty(),
            format!("{} 个", ids.len()),
        );
        report.assert_hard(
            &format!("轮{i} 内容正确（含 {kw}）"),
            text.contains(*kw),
            format!("拼接文本头 {:?}", text.chars().take(40).collect::<String>()),
        );
    }
    assert_hard_all(report).await;
}

// 负载均衡场景默认 ignore（多入口轮换/跨入口续传已验证通过；重跑须明确
// 决定）：cargo test -p rcoder-e2e --test k8s_lb -- --ignored --test-threads=1
#[tokio::test]
#[ignore = "lb 专项默认关闭（前轮已验证），确认后用 --ignored 显式跑"]
async fn lb_entry_rotation() {
    scenario_entry_rotation().await;
}

#[tokio::test]
#[ignore = "lb 专项默认关闭（前轮已验证），确认后用 --ignored 显式跑"]
async fn lb_cross_entry_cursor_reconnect() {
    scenario_cross_entry_cursor_reconnect().await;
}

#[tokio::test]
#[ignore = "lb 专项默认关闭（前轮已验证），确认后用 --ignored 显式跑"]
async fn lb_new_session_cross_entry() {
    scenario_new_session_cross_entry().await;
}

/// A normal Computer Stop must allow the SDK to load the same persisted SID.
#[tokio::test]
#[ignore = "explicit K8s real-AI environment"]
async fn lb_stop_resume_same_sid_cross_entry() {
    let scenario = "lb_stop_resume_same_sid_cross_entry";
    let Some((env, report, entries)) = k8s_or_skip(scenario).await else {
        return;
    };
    let user = env.scoped_user("lsr");
    let secret = format!("remember_{}", uuid::Uuid::new_v4().simple());
    let after = format!("after_stop_{}", env.run_tag);
    let mut project_id = None;
    let outcome: anyhow::Result<()> = async {
        anyhow::ensure!(
            entries.len() > 1 && entries[0] != entries[1],
            "Stop/resume requires distinct K8s entries"
        );
        let first_request = env.base_payload(
            Backend::Openai,
            &format!("记住口令 {secret}。本轮只回复 READY_{secret}，不要调用工具。"),
            &format!("{}-before-stop", env.run_tag),
            &user,
        );
        let first =
            chat_reported(&env, &report, "before_stop", &entries[0], &first_request).await?;
        project_id = Some(first.project_id.clone());
        anyhow::ensure!(
            first.error.is_none() && first.need_fallback != Some(true),
            "Initial chat did not establish a usable session"
        );
        let (events, _) = collect_reported(
            &env,
            &report,
            CollectSpec {
                phase: "before_stop_cross_entry",
                entry: &entries[1],
                sid: &first.session_id,
                duration_s: 60.0,
                last_event_id: None,
                idle_stop: true,
            },
        )
        .await;
        report.assert_hard(
            "停止前真实对话完整执行",
            events.iter().any(|e| e.event == "end_turn")
                && sse::chunks_text(&events).contains(&format!("READY_{secret}")),
            format!("{} events", events.len()),
        );

        let response = env
            .http
            .post(format!(
                "{}/computer/agent/stop",
                entries[entries.len() - 1]
            ))
            .timeout(Duration::from_secs(60))
            .json(&json!({"user_id":user,"project_id":first.project_id,
                "session_id":first.session_id}))
            .send()
            .await?;
        let status = response.status();
        let body: serde_json::Value = response.json().await?;
        let stopped =
            status.is_success() && body["code"] == "0000" && body["data"]["success"] == true;
        report.assert_hard(
            "普通 Computer Stop 明确成功",
            stopped,
            format!("HTTP {status}, response: {body}"),
        );
        anyhow::ensure!(stopped, "Computer Stop failed before resume");

        // The remembered word is deliberately absent: a fresh unrelated
        // session cannot satisfy this context assertion from the new prompt.
        let mut resumed_request = env.base_payload(
            Backend::Openai,
            &format!("只输出 {after}::上一轮我让你记住的口令。用原口令替换中文说明，保留两个英文冒号，不要调用工具。"),
            &format!("{}-after-stop", env.run_tag),
            &user,
        );
        resumed_request.project_id = Some(first.project_id.clone());
        resumed_request.session_id = Some(first.session_id.clone());
        let resumed =
            chat_reported(&env, &report, "after_stop", &entries[1], &resumed_request).await?;
        report.assert_hard(
            "停止后原 SID 一次受理且无降级",
            resumed.error.is_none()
                && resumed.need_fallback != Some(true)
                && resumed.session_id == first.session_id
                && resumed.project_id == first.project_id,
            format!(
                "same SID: {}, same project: {}, fallback: {:?}",
                resumed.session_id == first.session_id,
                resumed.project_id == first.project_id,
                resumed.need_fallback
            ),
        );
        let (events, _) = collect_reported(
            &env,
            &report,
            CollectSpec {
                phase: "after_stop_cross_entry",
                entry: &entries[0],
                sid: &resumed.session_id,
                duration_s: 60.0,
                last_event_id: None,
                idle_stop: true,
            },
        )
        .await;
        let messages = sse::message_events(&events);
        report.assert_hard(
            "停止后跨入口 SSE 收到当前请求",
            !messages.is_empty()
                && messages.iter().all(|event| {
                    event.data["data"]["request_id"]
                        .as_str()
                        .is_none_or(|id| Some(id) == resumed_request.request_id.as_deref())
                })
                && events.iter().any(|event| {
                    event.event == "prompt_start"
                        && event.data["data"]["request_id"].as_str()
                            == resumed_request.request_id.as_deref()
                })
                && events.iter().any(|event| {
                    event.event == "end_turn"
                        && event.data["data"]["request_id"].as_str()
                            == resumed_request.request_id.as_deref()
                }),
            format!(
                "{} message events; start/end must match the resume request",
                messages.len()
            ),
        );
        report.assert_hard(
            "停止后原会话上下文保留",
            sse::chunks_text(&events).contains(&format!("{after}::{secret}")),
            "new receipt joins a new marker and a word absent from the resume prompt".into(),
        );
        report.assert_hard(
            "停止后新轮完整且没有错误事件",
            sse::chunks_text(&events).contains(&after)
                && events
                    .iter()
                    .filter(|event| event.event == "end_turn")
                    .count()
                    == 1
                && !events.iter().any(|event| event.event == "error"),
            format!("{} events", events.len()),
        );
        Ok(())
    }
    .await;
    report.assert_hard(
        "普通 Computer Stop 后原入口自动恢复",
        outcome.is_ok(),
        outcome
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "one resume POST, real model and cross-entry SSE completed".into()),
    );
    // Preserve data and use the platform's physical identity guards even when
    // the chat outcome was uncertain. Do not delete controllers by name here.
    let cleanup: anyhow::Result<()> = async {
        let response = env
            .http
            .post(format!("{}/computer/pod/stop", entries[0]))
            .timeout(Duration::from_secs(60))
            .json(&json!({"user_id":user,
                "project_id":project_id.as_deref().unwrap_or(&user),
                "service_type":"computer-agent-runner"}))
            .send()
            .await?;
        let status = response.status();
        let body: serde_json::Value = response.json().await?;
        anyhow::ensure!(
            status.is_success() && body["code"] == "0000",
            "Owned Computer cleanup failed: HTTP {status}, response: {body}"
        );
        Ok(())
    }
    .await;
    report.assert_hard(
        "本 case 计算资源受控停止",
        cleanup.is_ok(),
        cleanup
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| {
                "platform compute Stop confirmed; volume identities checked by outer inventory"
                    .into()
            }),
    );
    assert_hard_all(report).await;
}

// gate 冒烟：无 TEST_K8S_SSH 时 skip 路径可走通（本地 compose 机器验证用）。
#[tokio::test]
async fn gate_k8s_or_skip_smoke() {
    if let Some((_env, report, _entries)) = k8s_or_skip("gate_k8s_smoke").await {
        report.skip("smoke: gate passed, no scenario body");
    }
}

async fn scenario_userapp_parallel(backend: Backend) {
    use rcoder_e2e::common::userapp_agent_k8s::{BuilderProbe, scoped_app_id};
    let scenario = "lb_userapp_parallel_agents";
    let Some((env, report, entries)) = k8s_or_skip(scenario).await else {
        return;
    };
    let case = std::env::var("E2E_CASE_ID").expect("registered K8s case identity");
    let app = scoped_app_id(&case, backend).expect("legal per-case app ID");
    let user = env.scoped_user("uappar");
    let mut probe = match BuilderProbe::prepare(&env, &report, &app).await {
        Ok(probe) => probe,
        Err(error) => {
            report.assert_hard("K8s case 资源归属与创建前不存在", false, error.to_string());
            assert_hard_all(report).await;
            return;
        }
    };
    report.assert_hard("K8s case 资源归属与创建前不存在", true, app.clone());
    probe.mark_admission_attempt();
    let outcome = async {
        let response = env
            .http
            .post(format!("{}/api/v1/userapp/workspace", entries[0]))
            .timeout(Duration::from_secs(600))
            .json(&json!({"app_id":app,"user_id":user}))
            .send()
            .await?;
        let status = response.status();
        let body: serde_json::Value = response.json().await?;
        let ok = status.is_success() && body["success"] == true;
        probe.capture(&env, &report, ok).await?;
        report.assert_hard(
            "create-workspace（ensure 开发容器+建目录+owner 注册）",
            ok,
            format!("HTTP {status}"),
        );
        anyhow::ensure!(ok, "UserApp builder admission failed");
        rcoder_e2e::common::userapp_agent_parallel::verify(
            &env, &report, &app, &user, backend, &entries,
        )
        .await
    }
    .await;
    report.assert_hard(
        "并行会话主流程完成",
        outcome.is_ok(),
        outcome
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "real AI flow completed".into()),
    );
    let cleanup = probe.cleanup(&env, &report, &user, &entries[0]).await;
    report.assert_hard(
        "K8s 本 case builder 清理成功",
        cleanup.is_ok(),
        cleanup
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "controlled stop and retained volume evidence complete".into()),
    );
    assert_hard_all(report).await;
}

#[tokio::test]
#[ignore = "explicit K8s real-AI environment"]
async fn lb_userapp_parallel_agents_openai() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    scenario_userapp_parallel(Backend::Openai).await;
}

#[tokio::test]
#[ignore = "explicit K8s real-AI environment"]
async fn lb_userapp_parallel_agents_anthropic() {
    rcoder_e2e::common::cross_bin_lock::acquire();
    scenario_userapp_parallel(Backend::Anthropic).await;
}
