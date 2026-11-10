//! Real HTTP -> tonic restart boundaries; no model output is synthesized.
//!
//! 核心回归守卫：restart 与 stop 的本质差异是**保留会话注册**——
//! stop 清空 `sessions()`，restart 必须原样保留（供下次 chat resume）。
//! 本文件所有用例在“误调 stop 的清理函数”实现回归时必须失败。

use super::*;
use crate::handler::userapp_agent_target::test_support::{AgentCall, Fixture};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType};
use std::sync::Arc;
use tower::ServiceExt as _;

fn memory() -> Arc<crate::storage::ProjectStoreBackend> {
    let adapter =
        crate::storage::ProjectAdapter::new("restart-http-test".into(), "cluster.local".into());
    Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
        adapter,
    )))
}

async fn state(
    fixture: &Fixture,
    backend: Arc<crate::storage::ProjectStoreBackend>,
) -> Arc<AppState> {
    // fixture.state 已带 ControlledRuntime 与协议 target 绑定；仅替换
    // project 后端为独立内存库，避免 APP_ID 预注册干扰。
    let mut state = (*fixture.state).clone();
    state.projects = backend;
    // deploy-host 形态（--all-features）寻址走 published 注册表；与 stop
    // 测试同款，把 fixture 容器登记到本机回环，避免 ReachNotReady 500。
    #[cfg(feature = "deploy-host")]
    shared_types::published::register_direct(
        &builder_container(fixture).container_name,
        std::net::Ipv4Addr::LOCALHOST.into(),
    );
    Arc::new(state)
}

fn builder_container(fixture: &Fixture) -> ContainerBasicInfo {
    fixture
        .runtime
        .builder
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone()
}

async fn seed(state: &AppState, physical: ContainerBasicInfo, project: &str, sid: &str) {
    let mut info = ProjectAndContainerInfo::new(project.into());
    info.set_user_id(Some(format!("owner-{project}")));
    info.set_service_type(Some(ServiceType::ComputerAgentRunner));
    info.set_container(Some(physical));
    assert_eq!(
        state
            .projects
            .insert_project_registration_durable(project.into(), Arc::new(info))
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    let captured = state.get_project(project).unwrap();
    assert_eq!(
        state
            .append_session_for_container_durable(&captured, sid)
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/computer/agent/restart", post(computer_agent_restart))
        .route(
            "/computer/agent/status",
            post(crate::handler::computer_agent_status),
        )
        .route(
            "/computer/progress/{session_id}",
            get(crate::handler::computer_agent_progress_notification),
        )
        .with_state(state)
}

async fn post_json(app: Router, path: &str, body: serde_json::Value) -> serde_json::Value {
    let response = app
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "HTTP {status}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn restart_stops_agent_and_preserves_session_registration() {
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let project = format!("restart-project-{suffix}");
    let sid = format!("restart-sid-{suffix}");
    let state = state(&fixture, memory()).await;
    let physical = builder_container(&fixture);
    seed(&state, physical, &project, &sid).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), sid.clone());

    let result = post_json(
        router(state.clone()),
        "/computer/agent/restart",
        serde_json::json!({
            "user_id": format!("owner-{project}"), "project_id": project, "session_id": sid,
        }),
    )
    .await;

    assert_eq!(result["code"], "0000", "{result}");
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(result["data"]["success"], true, "{result}");
    // 保留会话原样返回，供前端确认上下文可恢复。
    assert_eq!(result["data"]["resumed_session_id"], sid, "{result}");
    // 核心回归守卫：restart 不得清空会话注册（stop 在此断言 is_empty）。
    assert!(
        state
            .get_project(&project)
            .unwrap()
            .sessions()
            .contains(&sid),
        "restart must preserve the session registration for resume"
    );
    // 真实 Stop RPC 恰好一次（agent_runner 侧执行身份已确认退出）。
    let calls = fixture.agent.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|x| matches!(x, AgentCall::Stop(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn restart_reports_the_generation_it_actually_stopped() {
    // Codex P1 确定性交错反例：读取会话与停止执行之间发生换代时，
    // 响应的 resumed_session_id 必须是实际被停止且保留的代次（B），
    // 而不是读取时点已过期的 A。
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let project = format!("restart-race-{suffix}");
    let sid_a = format!("restart-race-a-{suffix}");
    let sid_b = format!("restart-race-b-{suffix}");
    let state = state(&fixture, memory()).await;
    let physical = builder_container(&fixture);
    seed(&state, physical, &project, &sid_a).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), sid_a.clone());

    // 交错钩子：StopAgent 已受理、尚未停止执行时，新 chat 完成换代
    //（注册与执行都切到 B）。restart 请求本身不带 session_id。
    let state_for_hook = state.clone();
    let project_for_hook = project.clone();
    let agent_for_hook = fixture.agent.clone();
    let sid_b_for_hook = sid_b.clone();
    *fixture.agent.before_stop.lock().unwrap() = Some(Box::new(move || {
        Box::pin(async move {
            let captured = state_for_hook.get_project(&project_for_hook).unwrap();
            assert_eq!(
                state_for_hook
                    .append_session_for_container_durable(&captured, &sid_b_for_hook)
                    .await
                    .unwrap(),
                shared_types::persistence::PersistenceWriteOutcome::Committed
            );
            agent_for_hook
                .sessions
                .lock()
                .unwrap()
                .insert(project_for_hook.clone(), sid_b_for_hook);
        })
    }));

    let result = post_json(
        router(state.clone()),
        "/computer/agent/restart",
        serde_json::json!({
            "user_id": format!("owner-{project}"), "project_id": project,
        }),
    )
    .await;

    assert_eq!(result["code"], "0000", "{result}");
    assert_eq!(result["data"]["success"], true, "{result}");
    // 被停止的是 B（StopAgent 按 project 取当前执行），返回的保留会话
    // 必须也是 B——不是读取时点的旧代次 A。
    assert_eq!(
        result["data"]["resumed_session_id"], sid_b,
        "resumed session must be the generation actually stopped and preserved: {result}"
    );
    assert!(
        state
            .get_project(&project)
            .unwrap()
            .sessions()
            .contains(&sid_b),
        "the preserved registration must carry the stopped generation"
    );
    // 真实 Stop 恰好一次，且未携带 expected（无 session_id 的 restart 语义）。
    let calls = fixture.agent.calls.lock().unwrap();
    let stops: Vec<_> = calls
        .iter()
        .filter_map(|x| match x {
            AgentCall::Stop(req) => Some(req),
            _ => None,
        })
        .collect();
    assert_eq!(stops.len(), 1);
    assert!(
        stops[0].expected_session_id.is_none(),
        "caller omitted session_id: restart must stop the current generation"
    );
}

#[tokio::test]
async fn restart_reports_stopped_session_from_rpc_not_registry() {
    // Codex R2 P1 反例:调用方显式指定 A;StopAgent 确认停止 A 之后、
    // HTTP 响应之前,注册被换代为 B(after_stop 钩子)。响应必须返回
    // RPC 确认的 A(stopped_session_id),而非从注册表读到的 B。
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let project = format!("restart-postread-{suffix}");
    let sid_a = format!("restart-postread-a-{suffix}");
    let sid_b = format!("restart-postread-b-{suffix}");
    let state = state(&fixture, memory()).await;
    let physical = builder_container(&fixture);
    seed(&state, physical, &project, &sid_a).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), sid_a.clone());

    // after_stop 钩子:停止确认之后、响应之前,换代注册到 B。
    let state_for_hook = state.clone();
    let project_for_hook = project.clone();
    let sid_b_for_hook = sid_b.clone();
    *fixture.agent.after_stop.lock().unwrap() = Some(Box::new(move || {
        Box::pin(async move {
            let captured = state_for_hook.get_project(&project_for_hook).unwrap();
            assert_eq!(
                state_for_hook
                    .append_session_for_container_durable(&captured, &sid_b_for_hook)
                    .await
                    .unwrap(),
                shared_types::persistence::PersistenceWriteOutcome::Committed
            );
        })
    }));

    let result = post_json(
        router(state.clone()),
        "/computer/agent/restart",
        serde_json::json!({
            "user_id": format!("owner-{project}"), "project_id": project,
            "session_id": sid_a,
        }),
    )
    .await;

    assert_eq!(result["code"], "0000", "{result}");
    // 返回 RPC 确认停止的 A,而不是停止之后注册表里的 B。
    assert_eq!(
        result["data"]["resumed_session_id"], sid_a,
        "must report the session the RPC actually stopped, not a post-stop registry read: {result}"
    );
}

#[tokio::test]
async fn status_resource_query_rejects_stale_expected_identity() {
    // Codex R2 P2 反例:fixture 的 GetContainerStatus 核验 expected——
    // 预置资源在身份不符时必须缺省(异代拼装防线在采样端生效)。
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let project = format!("restart-stale-{suffix}");
    let sid_a = format!("restart-stale-a-{suffix}");
    let state = state(&fixture, memory()).await;
    let physical = builder_container(&fixture);
    seed(&state, physical, &project, &sid_a).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), sid_a.clone());
    *fixture.agent.container_status.lock().unwrap() =
        Some(shared_types::grpc::GetContainerStatusResponse {
            is_active: true,
            active_tasks: 1,
            uptime_seconds: 120,
            status: "Processing".into(),
            cpu_percent: Some(42.5),
            memory_mb: Some(384),
            idle_observation_version: 1,
            container_cpu_percent: Some(15.0),
            container_memory_mb: Some(1024),
            container_memory_limit_mb: Some(3954),
            container_cpu_limit_cores: Some(2.0),
            // 进程明细与分组聚合(0.1.419+ 新增字段;这里只做结构完整性)
            processes: Vec::new(),
            process_count: Some(0),
            infra_rss_bytes: Some(0),
            platform_rss_bytes: Some(0),
            workload_rss_bytes: Some(0),
            // 执行存在性(与资源降级无关的显式存活信号)
            agent_present: Some(true),
        });

    // 身份匹配(expected=A,registry=A):资源返回。
    let result = post_json(
        router(state.clone()),
        "/computer/agent/status",
        serde_json::json!({
            "user_id": format!("owner-{project}"), "project_id": project,
            "session_id": sid_a,
        }),
    )
    .await;
    assert_eq!(result["data"]["cpu_percent"], 42.5, "{result}");

    // 注册换代到 B(旧 chat 完成注册):再次查询仍显式指定 A——GetStatus
    // found=false(A 不再是当前执行)→ not_alive,资源不拼装。
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), format!("restart-stale-b-{suffix}"));
    let result = post_json(
        router(state.clone()),
        "/computer/agent/status",
        serde_json::json!({
            "user_id": format!("owner-{project}"), "project_id": project,
            "session_id": sid_a,
        }),
    )
    .await;
    assert_eq!(result["data"]["is_alive"], false, "{result}");
    assert!(
        result["data"]["cpu_percent"].is_null(),
        "stale identity must not receive resources: {result}"
    );
}

#[tokio::test]
async fn restart_is_idempotent_when_agent_is_absent() {
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let project = format!("restart-idem-{suffix}");
    let sid = format!("restart-idem-sid-{suffix}");
    let state = state(&fixture, memory()).await;
    let physical = builder_container(&fixture);
    seed(&state, physical, &project, &sid).await;
    // 不向 fixture.agent.sessions 注册：mock StopAgent 返回 not_found
    //（agent 本就不在运行）。

    let result = post_json(
        router(state.clone()),
        "/computer/agent/restart",
        serde_json::json!({
            "user_id": format!("owner-{project}"), "project_id": project,
        }),
    )
    .await;

    assert_eq!(result["code"], "0000", "{result}");
    assert_eq!(result["data"]["success"], true, "{result}");
    // 幂等成功仍返回保留会话（注册未被动过）。
    assert_eq!(result["data"]["resumed_session_id"], sid, "{result}");
    assert!(
        state
            .get_project(&project)
            .unwrap()
            .sessions()
            .contains(&sid),
        "idempotent restart must not touch the registration either"
    );
}

#[tokio::test]
async fn restart_session_mismatch_returns_agent_not_found() {
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let project = format!("restart-mismatch-{suffix}");
    let live_sid = format!("live-{suffix}");
    let state = state(&fixture, memory()).await;
    let physical = builder_container(&fixture);
    seed(&state, physical, &project, &live_sid).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), live_sid.clone());

    // 携带与执行身份不符的 expected session：定位与执行不一致，拒绝重启。
    let result = post_json(
        router(state.clone()),
        "/computer/agent/restart",
        serde_json::json!({
            "user_id": format!("owner-{project}"), "project_id": project,
            "session_id": format!("other-{suffix}"),
        }),
    )
    .await;

    assert_eq!(result["success"], false, "{result}");
    assert_eq!(
        result["code"],
        shared_types::ERR_AGENT_NOT_FOUND,
        "{result}"
    );
    // 拒绝路径同样不得清注册。
    assert!(
        state
            .get_project(&project)
            .unwrap()
            .sessions()
            .contains(&live_sid),
        "a rejected restart must preserve the registration"
    );
}

#[tokio::test]
async fn restart_without_container_returns_container_not_found() {
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let owner = format!("restart-orphan-user-{suffix}");
    let project = format!("restart-orphan-{suffix}");
    let state = state(&fixture, memory()).await;
    // 不注册 project 映射：user_id 回退查找 ControlledRuntime.get_container_info
    //（恒 None）→ 容器不存在。

    let result = post_json(
        router(state.clone()),
        "/computer/agent/restart",
        serde_json::json!({
            "user_id": owner, "project_id": project,
        }),
    )
    .await;

    assert_eq!(result["success"], false, "{result}");
    assert_eq!(
        result["code"],
        shared_types::ERR_CONTAINER_NOT_FOUND,
        "{result}"
    );
}

#[tokio::test]
async fn status_endpoint_reports_agent_resource_usage() {
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let project = format!("restart-res-{suffix}");
    let sid = format!("restart-res-sid-{suffix}");
    let state = state(&fixture, memory()).await;
    let physical = builder_container(&fixture);
    seed(&state, physical, &project, &sid).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), sid.clone());
    // 预配置容器状态响应（agent_runner 采集填充的资源字段）。
    *fixture.agent.container_status.lock().unwrap() =
        Some(shared_types::grpc::GetContainerStatusResponse {
            is_active: true,
            active_tasks: 1,
            uptime_seconds: 120,
            status: "Processing".into(),
            cpu_percent: Some(42.5),
            memory_mb: Some(384),
            idle_observation_version: 1,
            container_cpu_percent: Some(15.0),
            container_memory_mb: Some(1024),
            container_memory_limit_mb: Some(3954),
            container_cpu_limit_cores: Some(2.0),
            // 进程明细与分组聚合(0.1.419+ 新增字段;这里只做结构完整性)
            processes: Vec::new(),
            process_count: Some(0),
            infra_rss_bytes: Some(0),
            platform_rss_bytes: Some(0),
            workload_rss_bytes: Some(0),
            // 执行存在性(与资源降级无关的显式存活信号)
            agent_present: Some(true),
        });

    let result = post_json(
        router(state.clone()),
        "/computer/agent/status",
        serde_json::json!({
            "user_id": format!("owner-{project}"), "project_id": project, "session_id": sid,
        }),
    )
    .await;

    assert_eq!(result["code"], "0000", "{result}");
    assert_eq!(result["data"]["is_alive"], true, "{result}");
    // 资源字段从 GetContainerStatus 透传（agent 进程组口径）。
    assert_eq!(result["data"]["cpu_percent"], 42.5, "{result}");
    assert_eq!(result["data"]["memory_mb"], 384, "{result}");
}

#[tokio::test]
async fn status_endpoint_degrades_resource_fields_on_grpc_unimplemented() {
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let project = format!("restart-res-deg-{suffix}");
    let sid = format!("restart-res-deg-sid-{suffix}");
    let state = state(&fixture, memory()).await;
    let physical = builder_container(&fixture);
    seed(&state, physical, &project, &sid).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), sid.clone());
    // container_status 未预配置 → mock 返回 unimplemented → 资源字段降级
    // None，状态查询本身仍成功（降级不 500 契约）。

    let result = post_json(
        router(state.clone()),
        "/computer/agent/status",
        serde_json::json!({
            "user_id": format!("owner-{project}"), "project_id": project, "session_id": sid,
        }),
    )
    .await;

    assert_eq!(result["code"], "0000", "{result}");
    assert_eq!(result["data"]["is_alive"], true, "{result}");
    assert!(
        result["data"]["cpu_percent"].is_null(),
        "degraded resource must be absent: {result}"
    );
    assert!(
        result["data"]["memory_mb"].is_null(),
        "degraded resource must be absent: {result}"
    );
}
