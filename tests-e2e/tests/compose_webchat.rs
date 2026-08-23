//! compose 环境 `/chat`（Web Agent Runner 域）集成测试。
//!
//! 与 computer 域（compose_session/compose_sse）互补的另一半 chat 链路：
//! - `/chat` 是三层架构里 Java backend 调 Web 项目对话的主入口（project_id 语义，
//!   缺省自动生成——区别于 computer 域的 user_id 沙箱语义）
//! - 进度流走 `/agent/progress/{session_id}`（web 域端点；computer 域是
//!   `/computer/progress/{sid}`——同 impl 不同路由上下文）
//!
//! 覆盖点：
//! - full_turn：project_id **自动生成回显**（/chat 特有语义锚点）+ SSE 完整轮
//! - project_reuse：同 project_id 二次 chat 的会话连续性语义
//! - two_turn_seq：第二轮 seq 大于首轮全部（对齐 computer 域断言模式）

use std::time::Duration;

use rcoder_e2e::common::scenario::{assert_hard_all, collect_reported_web, count_event};
use rcoder_e2e::common::{
    Backend, Env, TestUserGuard, base_payload_web, chat_web_reported, cross_bin_lock,
};

/// WebAgentRunner 容器按 project_id 命名（compose 前缀 dev-master-rcoder-{project_id}）；
/// TestUserGuard 只清 agent-runner 前缀，本域需自理。
fn cleanup_project(project_id: &str) {
    let name = format!("dev-master-rcoder-{project_id}");
    drop(
        std::process::Command::new("docker")
            .args(["rm", "-f", &name])
            .output(),
    );
}

/// WebAgentRunner 容器清理守卫（Drop 语义，panic 安全——对齐 TestUserGuard 模式；
/// TestUserGuard 只清 agent-runner 前缀，管不到 dev-master-rcoder-{project_id}）。
struct ProjectGuard {
    project_id: String,
}

impl Drop for ProjectGuard {
    fn drop(&mut self) {
        cleanup_project(&self.project_id);
    }
}

/// full_turn 骨架（两后端共用）：不带 project_id → 自动生成回显 + SSE 完整轮。
async fn webchat_full_turn(backend: &str) {
    let scenario = "webchat_full_turn";
    cross_bin_lock::acquire();
    let Some((env, report)) = Env::compose_or_skip(scenario, backend).await else {
        return;
    };
    let _user_guard = TestUserGuard::new(&env, "webchat-f");

    let backend_enum = if backend == "anthropic" {
        Backend::Anthropic
    } else {
        Backend::Openai
    };
    let req = base_payload_web(
        &env,
        backend_enum,
        "从1数到6，每行一个数字",
        &format!("{}-f1", env.run_tag),
    );
    let Ok(data) = chat_web_reported(&env, &report, "turn1", &req).await else {
        assert_hard_all(report).await;
        if let Some(pid) = req.project_id {
            cleanup_project(&pid);
        }
        return;
    };
    let sid = data.session_id.clone();
    report.assert_hard("session_id 非空", !sid.is_empty(), sid.clone());
    // /chat 特有：未传 project_id → 自动生成回显
    report.assert_hard(
        "project_id 自动生成回显",
        !data.project_id.is_empty(),
        format!("回显 {:?}", data.project_id),
    );
    let project = data.project_id.clone();

    tokio::time::sleep(Duration::from_millis(800)).await;
    let (events, _) =
        collect_reported_web(&env, &report, "collect_turn1", &env.rcoder, &sid, 30.0).await;
    report.assert_hard(
        "含 prompt_start",
        count_event(&events, "prompt_start") >= 1,
        format!("{events:?}"),
    );
    report.assert_hard(
        "含 end_turn（完整轮）",
        count_event(&events, "end_turn") >= 1,
        format!("{events:?}"),
    );
    let chunk_count = events
        .iter()
        .filter(|e| e.event.contains("message") || e.event.contains("chunk"))
        .count();
    report.assert_hard(
        "含流式消息事件",
        chunk_count >= 1,
        format!("count={chunk_count}"),
    );
    cleanup_project(&project);
    assert_hard_all(report).await;
}

#[tokio::test]
async fn webchat_full_turn_openai() {
    webchat_full_turn("openai").await;
}

#[tokio::test]
async fn webchat_full_turn_anthropic() {
    webchat_full_turn("anthropic").await;
}

/// 同 project_id 二次 chat：会话连续性（返回 session 复用或新建均可，但映射一致）。
#[tokio::test]
async fn webchat_project_reuse() {
    let scenario = "webchat_project_reuse";
    cross_bin_lock::acquire();
    let Some((env, report)) = Env::compose_or_skip(scenario, "openai").await else {
        return;
    };
    let _user_guard = TestUserGuard::new(&env, "webchat-r");

    let mut req1 = base_payload_web(
        &env,
        Backend::Openai,
        "回复数字1",
        &format!("{}-r1", env.run_tag),
    );
    // 显式 project_id（复用语义的前提）
    req1.project_id = Some(format!("{}-webchat-reuse", env.run_tag));
    let pid = req1.project_id.clone().unwrap();
    let _project_guard = ProjectGuard {
        project_id: pid.clone(),
    };
    let d1 = match chat_web_reported(&env, &report, "turn1", &req1).await {
        Ok(d) => d,
        Err(_) => {
            assert_hard_all(report).await;
            cleanup_project(&pid);
            return;
        }
    };
    let mut req2 = base_payload_web(
        &env,
        Backend::Openai,
        "回复数字2",
        &format!("{}-r2", env.run_tag),
    );
    req2.project_id = Some(pid.clone());
    let Ok(d2) = chat_web_reported(&env, &report, "turn2", &req2).await else {
        assert_hard_all(report).await;
        return;
    };
    // 语义锚点：两轮同 project → 会话归属一致（session 复用时相等；新建时
    // project 回显仍须一致——锁 /chat 的 project 维度路由稳定性）
    report.assert_hard(
        "两轮 project_id 一致",
        d1.project_id == d2.project_id,
        format!("t1={:?} t2={:?}", d1.project_id, d2.project_id),
    );
    report.assert_hard(
        "turn2 session_id 非空",
        !d2.session_id.is_empty(),
        d2.session_id.clone(),
    );
    assert_hard_all(report).await;
}

/// 两轮 seq 隔离（对齐 computer 域断言模式）。
#[tokio::test]
async fn webchat_two_turn_seq() {
    let scenario = "webchat_two_turn_seq";
    cross_bin_lock::acquire();
    let Some((env, report)) = Env::compose_or_skip(scenario, "openai").await else {
        return;
    };
    let _user_guard = TestUserGuard::new(&env, "webchat-s2");

    let mut req1 = base_payload_web(
        &env,
        Backend::Openai,
        "从1数到3，每行一个数字",
        &format!("{}-s1", env.run_tag),
    );
    req1.project_id = Some(format!("{}-webchat-seq", env.run_tag));
    let pid = req1.project_id.clone().unwrap();
    let _project_guard = ProjectGuard {
        project_id: pid.clone(),
    };
    let Ok(d1) = chat_web_reported(&env, &report, "turn1", &req1).await else {
        assert_hard_all(report).await;
        return;
    };
    let sid1 = d1.session_id.clone();
    let (e1, ended1) = collect_reported_web(&env, &report, "c1", &env.rcoder, &sid1, 25.0).await;

    let mut req2 = base_payload_web(
        &env,
        Backend::Openai,
        "从4数到6，每行一个数字",
        &format!("{}-s2", env.run_tag),
    );
    req2.project_id = Some(pid.clone());
    req2.session_id = Some(sid1.clone());
    let Ok(d2) = chat_web_reported(&env, &report, "turn2", &req2).await else {
        assert_hard_all(report).await;
        return;
    };
    let sid2 = d2.session_id.clone();
    let (e2, _) = collect_reported_web(&env, &report, "c2", &env.rcoder, &sid2, 25.0).await;

    report.assert_hard(
        "turn1 有事件",
        !e1.is_empty(),
        format!("events={} ended={ended1:?}", e1.len()),
    );
    report.assert_hard(
        "turn2 有事件",
        !e2.is_empty(),
        format!("events={}", e2.len()),
    );
    // seq 隔离（对齐 computer 域断言模式）：第二轮全部 id > 第一轮最大 id
    let ids1: Vec<u64> = e1.iter().filter_map(|e| e.seq).collect();
    let ids2: Vec<u64> = e2.iter().filter_map(|e| e.seq).collect();
    if let (Some(max1), Some(min2)) = (ids1.iter().max(), ids2.iter().min()) {
        report.assert_hard(
            "第二轮 seq 全 > 首轮最大",
            min2 > max1,
            format!("max1={max1} min2={min2}"),
        );
    } else {
        report.assert_hard(
            "两轮均含带 seq 事件",
            !ids1.is_empty() && !ids2.is_empty(),
            format!("ids1={} ids2={}", ids1.len(), ids2.len()),
        );
    }
    report.assert_hard(
        "turn2 含 end_turn",
        count_event(&e2, "end_turn") >= 1,
        format!("events={}", e2.len()),
    );
    assert_hard_all(report).await;
}
