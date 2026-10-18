//! 受控协议回归：实际 gRPC handler→chat core→ACP 子进程，不是 AI 验收。
#![cfg(unix)]

use agent_runner::agent_mgmt::{AgentRegistry, PathManager};
use agent_runner::{
    api_key_manager::ApiKeyManager,
    config::AppConfig,
    grpc::AgentServiceImpl,
    router::AppState,
    service::{AGENT_REGISTRY, AgentSessionService},
};
use dashmap::DashMap;
use serde_json::Value;
use shared_types::grpc::{
    CancelRequest, ChatAgentConfig, ChatAgentServerConfig, ChatRequest, GetStatusRequest,
    ProgressRequest, StopAgentRequest, agent_service_server::AgentService,
};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio_stream::StreamExt;
use tonic::Request;

fn rows(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("protocol.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn wait_row(root: &Path, event: &str, project: &str, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if rows(root)
                .iter()
                .filter(|row| row["event"] == event && row["project"] == project)
                .count()
                >= count
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "fixture event {event}/{project}/{count} missing: {:?}",
            rows(root)
        )
    });
}

fn chat(root: &Path, project: &str, sid: &str, fresh: bool, prompt: &str) -> ChatRequest {
    ChatRequest {
        project_id: project.into(),
        session_id: sid.into(),
        prompt: prompt.into(),
        request_id: Some(uuid::Uuid::new_v4().to_string()),
        force_new_session: fresh,
        // 两个会话共享相同源码目录；使用 computer absolute cwd 免修改进程全局 env。
        service_type: Some(shared_types::ServiceType::ComputerAgentRunner.to_string()),
        agent_work_dir: Some(root.to_string_lossy().into_owned()),
        agent_config: Some(ChatAgentConfig {
            agent_server: Some(ChatAgentServerConfig {
                agent_id: Some("controlled-protocol-fixture".into()),
                command: Some(
                    which::which("python3")
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                ),
                args: vec![format!(
                    "{}/tests/fixtures/controlled_acp.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: HashMap::from([(
                    "ACP_FIXTURE_DIR".into(),
                    root.to_string_lossy().into_owned(),
                )]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn stop(service: &AgentServiceImpl, project: &str, sid: &str) {
    let result = service
        .stop_agent(Request::new(StopAgentRequest {
            project_id: project.into(),
            expected_session_id: Some(sid.into()),
            force: true,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(result.success, "stop result: {result:?}");
}

async fn service(root: &Path, create_budget: u64) -> AgentServiceImpl {
    let path_manager = PathManager::new_with_root(root.join("install"));
    let agent_mgmt_registry = Arc::new(AgentRegistry::load(path_manager.clone()).await.unwrap());
    let keys = Arc::new(DashMap::new());
    AgentServiceImpl::new(Arc::new(AppState {
        sessions: Arc::new(DashMap::new()),
        config: AppConfig::default(),
        agent_session_service: Arc::new(AgentSessionService::new(
            agent_abstraction::launcher::direct_model_runtime_env_resolver(),
            create_budget,
        )),
        #[cfg(feature = "proxy")]
        pingora_service: None,
        api_key_manager: Arc::new(ApiKeyManager::from_shared(keys.clone())),
        shared_api_key_manager: keys,
        project_uuid_map: Arc::new(DashMap::new()),
        agent_mgmt_registry,
        agent_mgmt_path_manager: path_manager,
    }))
}

#[tokio::test]
async fn preparation_project_status_is_busy_without_a_session_and_sid_queries_stay_exact() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 10).await;
    let project = format!("protocol-preparation-status-{}", uuid::Uuid::new_v4());
    let foreign = format!("protocol-real-status-{}", uuid::Uuid::new_v4());
    let service_ref = &service;
    let query = move |project: String, sid: String| async move {
        service_ref
            .get_status(Request::new(GetStatusRequest {
                project_id: project,
                session_id: sid,
            }))
            .await
            .unwrap()
            .into_inner()
    };
    let guard = agent_runner::service::PendingGuard::new(&AGENT_REGISTRY, &project);
    let initial = query(project.clone(), String::new()).await;
    let has_real_info = AGENT_REGISTRY.get_agent_info(&project).is_some();
    let published_sid = AGENT_REGISTRY.get_session_by_project(&project);
    let legacy = query(project.clone(), "pending".into()).await;
    let missing = query(project.clone(), "missing-real-id".into()).await;

    let real = service
        .chat(Request::new(chat(root.path(), &foreign, "", true, "hold")))
        .await
        .unwrap()
        .into_inner();
    if real.success {
        wait_row(root.path(), "prompt", &foreign, 1).await;
    }
    let wrong_owner = query(project.clone(), real.session_id.clone()).await;
    let real_active = query(foreign.clone(), real.session_id.clone()).await;
    AGENT_REGISTRY.try_update_agent_info(&foreign, |info| {
        info.status = shared_types::AgentStatus::Idle;
        true
    });
    let real_idle = query(foreign.clone(), String::new()).await;
    AGENT_REGISTRY.try_update_agent_info(&foreign, |info| {
        info.status = shared_types::AgentStatus::Active;
        true
    });
    if real.success {
        stop(&service, &foreign, &real.session_id).await;
    }
    drop(guard);
    let released = query(project.clone(), String::new()).await;
    AGENT_REGISTRY.set_pending(&project);
    let manual = query(project.clone(), String::new()).await;
    AGENT_REGISTRY.clear_pending_if_exists(&project);
    let cleared = query(project, String::new()).await;

    assert!(initial.is_found);
    assert_eq!(initial.status, "busy");
    assert_eq!(initial.stream_epoch, None);
    assert!(!has_real_info);
    assert_eq!(published_sid, None);
    for absent in [legacy, missing, wrong_owner, released, cleared] {
        assert!(
            !absent.is_found,
            "session and released-project queries must remain exact: {absent:?}"
        );
        assert_eq!(absent.status, "not_found");
        assert_eq!(absent.stream_epoch, None);
    }
    assert!(manual.is_found);
    assert_eq!(manual.status, "busy");
    assert_eq!(manual.stream_epoch, None);
    assert!(real.success, "real session failed: {real:?}");
    assert!(real_active.is_found);
    assert_eq!(real_active.status, "busy");
    assert!(real_idle.is_found);
    assert_eq!(real_idle.status, "idle");
}

#[tokio::test]
async fn auto_reload_without_registered_session_creates_new_sessions_for_each_project() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 10).await;
    let mut responses = Vec::new();
    for project in ["protocol-auto-reload-new-a", "protocol-auto-reload-new-b"] {
        let mut request = chat(root.path(), project, "", false, "hold");
        request.agent_config.as_mut().unwrap().auto_reload =
            Some(shared_types::grpc::AutoReloadConfig { enabled: true });
        let response = service
            .chat(Request::new(request))
            .await
            .unwrap()
            .into_inner();
        if response.success {
            wait_row(root.path(), "prompt", project, 1).await;
            stop(&service, project, &response.session_id).await;
        }
        responses.push((project, response));
    }
    let protocol = rows(root.path());
    for (project, response) in &responses {
        assert!(response.success, "new project chat failed: {response:?}");
        let created: Vec<_> = protocol
            .iter()
            .filter(|row| row["event"] == "new" && row["project"] == *project)
            .collect();
        assert_eq!(
            created.len(),
            1,
            "a preparation reservation is not a historical ACP session: {protocol:?}"
        );
        assert_eq!(created[0]["session"], response.session_id);
        assert!(
            !protocol
                .iter()
                .any(|row| row["event"] == "load" && row["project"] == *project)
        );
        assert_eq!(
            protocol
                .iter()
                .filter(|row| row["event"] == "prompt" && row["project"] == *project)
                .count(),
            1
        );
    }
    assert_ne!(responses[0].1.session_id, responses[1].1.session_id);
}

#[tokio::test]
async fn auto_reload_resumes_a_real_pending_session_and_force_new_still_creates() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 10).await;
    let project = "protocol-auto-reload-real-pending";
    let first = service
        .chat(Request::new(chat(root.path(), project, "", true, "hold")))
        .await
        .unwrap()
        .into_inner();
    assert!(first.success, "first: {first:?}");
    wait_row(root.path(), "prompt", project, 1).await;
    let original = AGENT_REGISTRY
        .view_agent_info(project, |info| info.stop_handle.clone().unwrap())
        .unwrap();
    AGENT_REGISTRY.try_update_agent_info(project, |info| {
        info.status = shared_types::AgentStatus::Pending;
        true
    });
    let mut resume = chat(root.path(), project, "", false, "hold");
    resume.agent_config.as_mut().unwrap().auto_reload =
        Some(shared_types::grpc::AutoReloadConfig { enabled: true });
    let resumed = service
        .chat(Request::new(resume))
        .await
        .unwrap()
        .into_inner();
    assert!(resumed.success, "resume: {resumed:?}");
    wait_row(root.path(), "prompt", project, 2).await;
    let mut fresh = chat(root.path(), project, &resumed.session_id, true, "hold");
    fresh.agent_config.as_mut().unwrap().auto_reload =
        Some(shared_types::grpc::AutoReloadConfig { enabled: true });
    let replaced = service
        .chat(Request::new(fresh))
        .await
        .unwrap()
        .into_inner();
    assert!(replaced.success, "fresh: {replaced:?}");
    wait_row(root.path(), "prompt", project, 3).await;
    stop(&service, project, &replaced.session_id).await;
    assert_eq!(
        resumed.session_id, first.session_id,
        "a real session remains resumable while Pending"
    );
    assert!(
        original.is_stopped(),
        "reload confirms the original execution exited"
    );
    assert_ne!(replaced.session_id, resumed.session_id);
    let protocol = rows(root.path());
    assert_eq!(
        protocol.iter().filter(|row| row["event"] == "new").count(),
        2
    );
    assert_eq!(
        protocol.iter().filter(|row| row["event"] == "load").count(),
        1
    );
    assert_eq!(
        protocol
            .iter()
            .filter(|row| row["event"] == "prompt")
            .count(),
        3,
        "accepted prompts are never replayed"
    );
    assert!(!AGENT_REGISTRY.is_pending(project));
}

#[tokio::test]
async fn legacy_pending_input_is_unspecified_and_never_sent_to_session_load() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 10).await;
    let project = "protocol-legacy-pending-input";
    let response = service
        .chat(Request::new(chat(
            root.path(),
            project,
            "pending",
            false,
            "hold",
        )))
        .await
        .unwrap()
        .into_inner();
    if response.success {
        wait_row(root.path(), "prompt", project, 1).await;
        stop(&service, project, &response.session_id).await;
    }
    assert!(response.success, "legacy input: {response:?}");
    let protocol = rows(root.path());
    let created: Vec<_> = protocol
        .iter()
        .filter(|row| row["event"] == "new")
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0]["session"], response.session_id);
    assert!(!protocol.iter().any(|row| row["event"] == "load"));
    assert_eq!(
        protocol
            .iter()
            .filter(|row| row["event"] == "prompt")
            .count(),
        1
    );
    assert!(!AGENT_REGISTRY.is_pending(project));
}

#[tokio::test]
async fn separate_projects_shared_cwd_foreign_hint_fresh_and_confirmed_stop() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 10).await;
    let a = "protocol-a";
    let b = "protocol-b";
    let first = service
        .chat(Request::new(chat(root.path(), a, "", true, "hold")))
        .await
        .unwrap()
        .into_inner();
    assert!(first.success, "first: {first:?}");
    wait_row(root.path(), "prompt", a, 1).await;
    let old_a = AGENT_REGISTRY
        .view_agent_info(a, |info| info.stop_handle.clone().unwrap())
        .unwrap();
    // 错把 A 的 sid 发给 B：只新建 B，不能复用/取消 A。
    let second = service
        .chat(Request::new(chat(
            root.path(),
            b,
            &first.session_id,
            false,
            "hold",
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(second.success, "second: {second:?}");
    assert_ne!(first.session_id, second.session_id);
    wait_row(root.path(), "prompt", b, 1).await;
    assert!(!old_a.is_stopped());
    assert!(
        !rows(root.path())
            .iter()
            .any(|row| row["event"] == "cancel" && row["project"] == a)
    );
    let status = service
        .get_status(Request::new(GetStatusRequest {
            project_id: b.into(),
            session_id: first.session_id.clone(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(!status.is_found, "foreign status must not return A");
    assert!(
        service
            .cancel_session(Request::new(CancelRequest {
                project_id: b.into(),
                session_id: first.session_id.clone(),
                reason: "foreign".into()
            }))
            .await
            .is_err()
    );
    let foreign_stop = service
        .stop_agent(Request::new(StopAgentRequest {
            project_id: b.into(),
            expected_session_id: Some(first.session_id.clone()),
            force: true,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(!foreign_stop.success);
    assert!(!old_a.is_stopped());
    let cancelled = service
        .cancel_session(Request::new(CancelRequest {
            project_id: b.into(),
            session_id: second.session_id.clone(),
            reason: "owned".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(cancelled.success);
    let owned_b = AGENT_REGISTRY
        .view_agent_info(b, |info| info.stop_handle.clone().unwrap())
        .unwrap();
    stop(&service, b, &second.session_id).await;
    assert!(
        owned_b.is_stopped(),
        "successful stop requires reaper completion"
    );
    assert!(!old_a.is_stopped(), "stopping B cannot stop A");

    let replacement = service
        .chat(Request::new(chat(root.path(), a, "", true, "finish")))
        .await
        .unwrap()
        .into_inner();
    assert!(replacement.success, "replacement: {replacement:?}");
    assert_ne!(replacement.session_id, first.session_id);
    assert!(
        old_a.is_stopped(),
        "fresh replacement waits for old child.wait"
    );
    wait_row(root.path(), "prompt_end", a, 1).await;
    let log = rows(root.path());
    let old_pid = log
        .iter()
        .find(|row| row["event"] == "spawn" && row["project"] == a)
        .unwrap()["pid"]
        .clone();
    let exit = log
        .iter()
        .position(|row| row["event"] == "exit" && row["pid"] == old_pid)
        .unwrap();
    let created = log
        .iter()
        .position(|row| row["event"] == "new" && row["session"] == replacement.session_id)
        .unwrap();
    assert!(
        exit < created,
        "old execution exits before new ACP session creation"
    );
    let resumed = service
        .chat(Request::new(chat(
            root.path(),
            a,
            &replacement.session_id,
            false,
            "finish",
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(resumed.success);
    assert_eq!(resumed.session_id, replacement.session_id);
    stop(&service, a, &resumed.session_id).await;
    // 内存注册表 miss 保留合法历史 LoadSession，而不是强制改成 fresh。
    let loaded = service
        .chat(Request::new(chat(
            root.path(),
            a,
            &resumed.session_id,
            false,
            "finish",
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(loaded.success, "historical load: {loaded:?}");
    assert_eq!(loaded.session_id, resumed.session_id);
    wait_row(root.path(), "load", a, 1).await;
    std::fs::write(root.path().join(format!("{a}.fail")), "fail").unwrap();
    let failed = service
        .chat(Request::new(chat(root.path(), a, "", true, "finish")))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !failed.success,
        "creation failure must not pretend the old session was reused"
    );
    assert!(
        failed
            .error
            .unwrap_or_default()
            .contains("controlled create failure")
    );
    assert!(
        !AGENT_REGISTRY.is_pending(a),
        "failed preparation releases only its reservation"
    );
    let expected_cwd = root.path().canonicalize().unwrap();
    assert!(
        rows(root.path())
            .iter()
            .filter(|row| row["event"] == "spawn")
            .all(|row| row["cwd"] == expected_cwd.to_string_lossy().as_ref()),
        "every subprocess must use the same physical shared workspace"
    );
}

async fn historical_fallback(marker: &str, project: &str) {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 2).await;
    std::fs::write(root.path().join(format!("{project}.{marker}")), marker).unwrap();
    let response = service
        .chat(Request::new(chat(
            root.path(),
            project,
            "owned-historical-session",
            false,
            "finish",
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(
        response.success,
        "LoadSession fallback must complete: {response:?}"
    );
    assert_eq!(response.project_id, project);
    assert_ne!(response.session_id, "owned-historical-session");
    assert!(!response.session_id.is_empty());
    wait_row(
        root.path(),
        if marker == "load-error" {
            "load_error"
        } else {
            "load_timeout"
        },
        project,
        1,
    )
    .await;
    wait_row(root.path(), "new", project, 1).await;
    wait_row(root.path(), "prompt_end", project, 1).await;
    let log = rows(root.path());
    let started = log
        .iter()
        .find(|row| row["event"] == "prompt" && row["project"] == project)
        .unwrap();
    assert_eq!(
        started["session"], response.session_id,
        "prompt uses the new session rather than the failed historical session"
    );
    let ended = log
        .iter()
        .find(|row| row["event"] == "prompt_end" && row["project"] == project)
        .unwrap();
    assert_eq!(ended["cancelled"], false);
    stop(&service, project, &response.session_id).await;
}

#[tokio::test]
async fn historical_load_error_falls_back_to_new_session_and_runs_prompt() {
    historical_fallback("load-error", "protocol-load-error").await;
}

#[tokio::test]
async fn historical_load_timeout_leaves_budget_for_new_session_and_prompt() {
    historical_fallback("load-timeout", "protocol-load-timeout").await;
}

#[tokio::test]
async fn delayed_successful_load_preserves_historical_session_within_its_budget() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 2).await;
    let project = "protocol-load-delay";
    std::fs::write(root.path().join(format!("{project}.load-delay")), "0.2").unwrap();
    let response = service
        .chat(Request::new(chat(
            root.path(),
            project,
            "owned-historical-session",
            false,
            "finish",
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(response.success, "delayed historical load: {response:?}");
    assert_eq!(response.project_id, project);
    assert_eq!(response.session_id, "owned-historical-session");
    wait_row(root.path(), "load", project, 1).await;
    wait_row(root.path(), "prompt_end", project, 1).await;
    assert!(
        !rows(root.path())
            .iter()
            .any(|row| row["event"] == "new" && row["project"] == project)
    );
    stop(&service, project, &response.session_id).await;
}

#[tokio::test]
async fn new_session_timeout_reports_failure_only_after_child_is_reaped() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 1).await;
    let project = "protocol-new-timeout";
    std::fs::write(root.path().join(format!("{project}.new-timeout")), "").unwrap();
    let response = service
        .chat(Request::new(chat(root.path(), project, "", true, "finish")))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !response.success,
        "startup timeout cannot return success: {response:?}"
    );
    assert_eq!(response.project_id, project);
    let log = rows(root.path());
    let spawned = log.iter().find(|row| row["event"] == "spawn").unwrap();
    let pid = spawned["pid"].as_u64().unwrap() as u32;
    assert!(log.iter().any(|row| row["event"] == "new_timeout"));
    // 超时终止可能使用 SIGKILL，fixture 没有机会写 SIGTERM exit 日志。
    // PID 连僵尸都不存在才证明真实 wait 已完成，必须先于下一次启动验证。
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH),
        "failure returned before actual child.wait"
    );
    assert!(!log.iter().any(|row| row["event"] == "prompt"));
    assert!(
        AGENT_REGISTRY
            .view_agent_info(project, Clone::clone)
            .is_none()
    );
    assert!(
        !AGENT_REGISTRY.is_pending(project),
        "failed startup cannot retain a preparation lease"
    );
    assert_eq!(log.iter().filter(|row| row["event"] == "spawn").count(), 1);
    std::fs::remove_file(root.path().join(format!("{project}.new-timeout"))).unwrap();
    let retry = service
        .chat(Request::new(chat(root.path(), project, "", true, "finish")))
        .await
        .unwrap()
        .into_inner();
    assert!(
        retry.success,
        "failed startup must permit corrected retry: {retry:?}"
    );
    assert_eq!(
        rows(root.path())
            .iter()
            .filter(|row| row["event"] == "spawn")
            .count(),
        2
    );
    stop(&service, project, &retry.session_id).await;
}

#[tokio::test]
async fn stopping_same_sid_replacement_waits_for_old_child_and_keeps_new_execution() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 3).await;
    let project = "protocol-stop-reload-race";
    let first = service
        .chat(Request::new(chat(root.path(), project, "", false, "hold")))
        .await
        .unwrap()
        .into_inner();
    assert!(first.success);
    wait_row(root.path(), "prompt", project, 1).await;
    std::fs::write(root.path().join(format!("{project}.term-delay")), "0.3").unwrap();
    let stopping = service.stop_agent(Request::new(StopAgentRequest {
        project_id: project.into(),
        expected_session_id: Some(first.session_id.clone()),
        force: true,
        ..Default::default()
    }));
    let replacing = async {
        wait_row(root.path(), "term_received", project, 1).await;
        service
            .chat(Request::new(chat(
                root.path(),
                project,
                &first.session_id,
                false,
                "hold",
            )))
            .await
    };
    let (stopped, replacement) = tokio::join!(stopping, replacing);
    assert!(stopped.unwrap().into_inner().success);
    let replacement = replacement.unwrap().into_inner();
    assert!(replacement.success, "same sid reload: {replacement:?}");
    assert_eq!(replacement.session_id, first.session_id);
    wait_row(root.path(), "prompt", project, 2).await;
    let log = rows(root.path());
    let old_exit = log.iter().position(|row| row["event"] == "exit").unwrap();
    let new_spawn = log
        .iter()
        .enumerate()
        .filter(|(_, row)| row["event"] == "spawn")
        .nth(1)
        .unwrap()
        .0;
    assert!(
        old_exit < new_spawn,
        "replacement spawned before old execution exited: {log:?}"
    );
    let target = AGENT_REGISTRY
        .view_agent_info(project, Clone::clone)
        .unwrap();
    assert_eq!(target.session_id.to_string(), first.session_id);
    assert!(
        !target.cancel_tx.is_closed(),
        "old Stop cleanup must keep replacement channel"
    );
    std::fs::remove_file(root.path().join(format!("{project}.term-delay"))).unwrap();
    stop(&service, project, &replacement.session_id).await;
}

async fn closed_active_runtime_recovers_history(project: &str, fail_load: bool) {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 3).await;
    let first = service
        .chat(Request::new(chat(root.path(), project, "", false, "hold")))
        .await
        .unwrap()
        .into_inner();
    assert!(first.success);
    wait_row(root.path(), "prompt", project, 1).await;
    let mut stale = AGENT_REGISTRY
        .view_agent_info(project, Clone::clone)
        .unwrap();
    let lifecycle = stale.stop_handle.clone().unwrap();
    let old_pid = rows(root.path())
        .iter()
        .find(|row| row["event"] == "spawn")
        .unwrap()["pid"]
        .as_u64()
        .unwrap() as i32;
    lifecycle.graceful_stop().await.unwrap();
    lifecycle.wait_for_exit().await.unwrap();
    // 等真实 connection receiver 与 watcher 结束，再注入故障留下的 Active 快照；
    // 快照里的 handle 与两个关闭通道都来自刚刚被真正回收的 ACP 子进程。
    tokio::time::timeout(Duration::from_secs(5), async {
        while !stale.cancel_tx.is_closed()
            || !stale.prompt_tx.is_closed()
            || AGENT_REGISTRY
                .view_agent_info(project, Clone::clone)
                .is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(old_pid), None),
        Err(nix::errno::Errno::ESRCH)
    );
    assert!(lifecycle.is_stopped());
    stale.status = shared_types::AgentStatus::Active;
    AGENT_REGISTRY.register(project, &first.session_id, stale);
    if fail_load {
        std::fs::write(root.path().join(format!("{project}.load-error")), "").unwrap();
    }

    let recovered = service
        .chat(Request::new(chat(
            root.path(),
            project,
            &first.session_id,
            false,
            "finish",
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(
        recovered.success,
        "closed Active runtime must reach historical recovery: {recovered:?}"
    );
    assert_eq!(recovered.project_id, project);
    if fail_load {
        assert_ne!(recovered.session_id, first.session_id);
        wait_row(root.path(), "load_error", project, 1).await;
        wait_row(root.path(), "new", project, 2).await;
    } else {
        assert_eq!(recovered.session_id, first.session_id);
        wait_row(root.path(), "load", project, 1).await;
    }
    wait_row(root.path(), "prompt", project, 2).await;
    let log = rows(root.path());
    let prompts: Vec<_> = log.iter().filter(|row| row["event"] == "prompt").collect();
    assert_eq!(prompts[1]["session"], recovered.session_id);
    assert_ne!(prompts[1]["pid"], old_pid);
    assert!(
        !log.iter()
            .any(|row| row["event"] == "cancel" && row["pid"] == old_pid),
        "closed old runtime must not receive a new cancellation"
    );
    stop(&service, project, &recovered.session_id).await;
}

#[tokio::test]
async fn closed_active_runtime_preserves_same_sid_historical_load() {
    closed_active_runtime_recovers_history("protocol-closed-active-load", false).await;
}

#[tokio::test]
async fn closed_active_runtime_load_failure_creates_new_session_and_runs_prompt() {
    closed_active_runtime_recovers_history("protocol-closed-active-new", true).await;
}

#[tokio::test]
async fn actual_acp_cancel_and_stop_stream_terminals_keep_accepted_request_identity() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 3).await;
    let project = "protocol-captured-cancel-rid";
    for stopping in [false, true] {
        let original_rid = if stopping {
            "original-stop-rid"
        } else {
            "original-cancel-rid"
        };
        let mut request = chat(root.path(), project, "", true, "hold");
        request.request_id = Some(original_rid.into());
        let response = service
            .chat(Request::new(request))
            .await
            .unwrap()
            .into_inner();
        assert!(response.success);
        wait_row(root.path(), "prompt", project, if stopping { 2 } else { 1 }).await;
        let mut stream = service
            .subscribe_progress(Request::new(ProgressRequest {
                session_id: response.session_id.clone(),
                from_seq: Some(0),
            }))
            .await
            .unwrap()
            .into_inner();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !agent_runner::service::SESSION_CACHE
                .view(&response.session_id, |_, data| data.connections_len() > 0)
                .unwrap_or(false)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let marker = if stopping {
            "term-delay"
        } else {
            "cancel-delay"
        };
        std::fs::write(root.path().join(format!("{project}.{marker}")), "0.2").unwrap();
        let control = async {
            if stopping {
                let result = service
                    .stop_agent(Request::new(StopAgentRequest {
                        project_id: project.into(),
                        expected_session_id: Some(response.session_id.clone()),
                        force: true,
                        ..Default::default()
                    }))
                    .await
                    .unwrap()
                    .into_inner();
                assert!(result.success);
            } else {
                let result = service
                    .cancel_session(Request::new(CancelRequest {
                        project_id: project.into(),
                        session_id: response.session_id.clone(),
                        reason: "controlled-rid".into(),
                    }))
                    .await
                    .unwrap()
                    .into_inner();
                assert!(result.success);
            }
        };
        let mutate_latest = async {
            wait_row(
                root.path(),
                if stopping {
                    "term_received"
                } else {
                    "cancel_received"
                },
                project,
                1,
            )
            .await;
            assert!(AGENT_REGISTRY.try_update_agent_info(project, |info| {
                info.request_id = Some("later-registry-request".into());
                true
            }));
        };
        tokio::join!(control, mutate_latest);
        let events = tokio::time::timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                let event = event.unwrap();
                events.push(event);
            }
            events
        })
        .await
        .unwrap();
        let cancelled: Vec<_> = events
            .iter()
            .filter(|event| event.sub_type == "cancelled")
            .collect();
        assert!(
            !cancelled.is_empty(),
            "control must end its original stream: stopping={stopping}, events={events:?}, protocol={:?}",
            rows(root.path())
        );
        for event in cancelled {
            assert_eq!(
                event.request_id.as_deref(),
                Some(original_rid),
                "terminal read a later registry request: {event:?}"
            );
            assert_eq!(
                serde_json::from_str::<Value>(&event.payload).unwrap()["request_id"],
                original_rid
            );
        }
        std::fs::remove_file(root.path().join(format!("{project}.{marker}"))).unwrap();
    }
}

#[tokio::test]
async fn genuine_acp_application_error_1002_remains_error_with_original_request_identity() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 3).await;
    let project = "protocol-agent-error-1002";
    std::fs::write(root.path().join(format!("{project}.prompt-error")), "hold").unwrap();
    let mut request = chat(root.path(), project, "", true, "finish");
    request.request_id = Some("application-error-rid".into());
    let response = service
        .chat(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert!(
        response.success,
        "session must start before the actual prompt error"
    );
    let mut stream = service
        .subscribe_progress(Request::new(ProgressRequest {
            session_id: response.session_id.clone(),
            from_seq: Some(0),
        }))
        .await
        .unwrap()
        .into_inner();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !agent_runner::service::SESSION_CACHE
            .view(&response.session_id, |_, data| data.connections_len() > 0)
            .unwrap_or(false)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    std::fs::write(
        root.path().join(format!("{project}.prompt-error-release")),
        "",
    )
    .unwrap();
    let events = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        events
    })
    .await
    .unwrap();
    assert!(
        !events.iter().any(|event| event.sub_type == "cancelled"),
        "numeric ACP error must not be reclassified as cancellation: {events:?}"
    );
    let error = events
        .iter()
        .find(|event| event.sub_type == "error")
        .unwrap();
    assert_eq!(error.request_id.as_deref(), Some("application-error-rid"));
    let payload: Value = serde_json::from_str(&error.payload).unwrap();
    assert_eq!(payload["code"], 1002);
    assert_eq!(payload["request_id"], "application-error-rid");
    assert!(
        payload["message"]
            .as_str()
            .unwrap()
            .contains("controlled agent application error")
    );
    stop(&service, project, &response.session_id).await;
}

#[tokio::test]
async fn actual_acp_abnormal_exit_after_cancel_keeps_rpc_failure_and_error_identity() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path(), 3).await;
    let project = "protocol-cancel-abnormal-exit";
    std::fs::write(
        root.path().join(format!("{project}.cancel-abnormal-exit")),
        "",
    )
    .unwrap();
    let mut request = chat(root.path(), project, "", true, "hold");
    request.request_id = Some("abnormal-cancel-rid".into());
    let response = service
        .chat(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert!(response.success);
    wait_row(root.path(), "prompt", project, 1).await;
    let mut stream = service
        .subscribe_progress(Request::new(ProgressRequest {
            session_id: response.session_id.clone(),
            from_seq: Some(0),
        }))
        .await
        .unwrap()
        .into_inner();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !agent_runner::service::SESSION_CACHE
            .view(&response.session_id, |_, data| data.connections_len() > 0)
            .unwrap_or(false)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let cancelled = service
        .cancel_session(Request::new(CancelRequest {
            project_id: project.into(),
            session_id: response.session_id.clone(),
            reason: "controlled abnormal exit".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !cancelled.success,
        "a child that dies before responding cannot confirm cancellation: {cancelled:?}"
    );
    wait_row(root.path(), "cancel_abnormal_exit", project, 1).await;
    let events = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        events
    })
    .await
    .unwrap();
    assert!(
        !events.iter().any(|event| event.sub_type == "cancelled"),
        "abnormal process failure must not become a successful cancelled terminal: {events:?}"
    );
    let error = events
        .iter()
        .find(|event| event.sub_type == "error")
        .unwrap();
    assert_eq!(error.request_id.as_deref(), Some("abnormal-cancel-rid"));
    assert_eq!(
        serde_json::from_str::<Value>(&error.payload).unwrap()["request_id"],
        "abnormal-cancel-rid"
    );
    stop(&service, project, &response.session_id).await;
}
