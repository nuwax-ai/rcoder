//! 真实 HTTP 下载和 handler 响应回归；失败 URL 不取消其余技能或工作区。
use super::*;
use crate::{
    AppState, BuildManager, Config, DevServerManager, LocalWorkspaceResolver, LogCacheManager,
};
use axum::{
    Router,
    body::Body,
    extract::{Path as HttpPath, State},
    http::{Request, StatusCode},
    routing::get,
};
use std::{io::Write as _, sync::Arc};
use tower::ServiceExt as _;

fn zip_fixture(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut archive = ::zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, value) in entries {
        archive
            .start_file(*name, ::zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(value.as_bytes()).unwrap();
    }
    archive.finish().unwrap().into_inner()
}
struct DownloadServer {
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for DownloadServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn downloads() -> DownloadServer {
    async fn serve(
        HttpPath(name): HttpPath<String>,
        State(files): State<Arc<std::collections::BTreeMap<String, Vec<u8>>>>,
    ) -> (StatusCode, Vec<u8>) {
        match files.get(&name) {
            Some(bytes) => (StatusCode::OK, bytes.clone()),
            None => (StatusCode::NOT_FOUND, b"missing skill".to_vec()),
        }
    }
    let files = Arc::new(std::collections::BTreeMap::from([
        (
            "good.zip".into(),
            zip_fixture(&[("skills/good/SKILL.md", "good skill")]),
        ),
        (
            "direct.zip".into(),
            zip_fixture(&[("direct/SKILL.md", "direct skill")]),
        ),
        (
            "empty.zip".into(),
            zip_fixture(&[("README.md", "no skill directories")]),
        ),
        ("broken.zip".into(), b"not a zip".to_vec()),
        (
            "blocked.zip".into(),
            zip_fixture(&[("blocked/SKILL.md", "blocked skill")]),
        ),
        (
            "escape.zip".into(),
            zip_fixture(&[("../outside/SKILL.md", "must not escape")]),
        ),
    ]));
    let router = Router::new().route("/{name}", get(serve)).with_state(files);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    DownloadServer { base, task }
}
fn state(root: &Path) -> AppState {
    let config = Arc::new(Config {
        project_source_dir: root.join("projects"),
        computer_workspace_dir: root.join("computer"),
        upload_project_dir: root.join("downloads"),
        skill_download_timeout_secs: 5,
        skill_download_connect_timeout_secs: 2,
        skill_url_allow_http: true,
        skill_url_allow_private_networks: true,
        ..Config::default()
    });
    AppState {
        resolver: Arc::new(LocalWorkspaceResolver::new(
            config.project_source_dir.clone(),
            config.computer_workspace_dir.clone(),
        )),
        dev_server: Arc::new(DevServerManager::new(config.clone())),
        build_manager: Arc::new(BuildManager::new(config.max_build_concurrency)),
        log_cache: Arc::new(LogCacheManager::new(&config)),
        skill_downloader: Arc::new(SkillDownloader::new(&config).unwrap()),
        config,
        preview: None,
        started_at: std::time::Instant::now(),
    }
}
async fn request(
    state: AppState,
    route: &str,
    fields: &[(&str, String)],
    file: Option<Vec<u8>>,
) -> (StatusCode, serde_json::Value) {
    let boundary = "skill-best-effort-fixture";
    let mut bytes = Vec::new();
    for (name, value) in fields {
        bytes.extend(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some(file) = file {
        bytes.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"skills.zip\"\r\nContent-Type: application/zip\r\n\r\n").as_bytes());
        bytes.extend(file);
        bytes.extend(b"\r\n");
    }
    bytes.extend(format!("--{boundary}--\r\n").as_bytes());
    let (router, _) = crate::routes::api_router().split_for_parts();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        router.with_state(state).oneshot(
            Request::post(route)
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(bytes))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn fields(project: bool, urls: Vec<String>, agent: bool) -> Vec<(&'static str, String)> {
    let mut values = if project {
        vec![("projectId", "p1".into())]
    } else {
        vec![("userId", "u1".into()), ("cId", "c1".into())]
    };
    values.push(("skillUrls", serde_json::to_string(&urls).unwrap()));
    if agent {
        values.push(("agentId", "agent1".into()));
    }
    values
}

#[tokio::test]
async fn skill_url_failures_project_and_computer_http_push_continue_and_report_each_failure() {
    let server = downloads().await;
    for project in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let ws = if project {
            state.config.project_source_dir.join("p1")
        } else {
            state.config.computer_workspace_dir.join("u1/c1")
        };
        fs::create_dir_all(&ws).await.unwrap();
        let urls = [
            "missing.zip",
            "broken.zip",
            "empty.zip",
            "escape.zip",
            "good.zip",
            "direct.zip",
        ]
        .map(|name| format!("{}/{name}", server.base))
        .to_vec();
        let route = if project {
            "/api/project/push-skills-to-workspace"
        } else {
            "/api/computer/push-skills-to-workspace"
        };
        let (status, result) = request(
            state.clone(),
            route,
            &fields(project, urls.clone(), false),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["success"], true);
        let failed = result["failedSkills"].as_array().unwrap();
        assert_eq!(failed.len(), 4);
        for (i, failure) in failed.iter().enumerate() {
            assert_eq!(failure["name"], serde_json::Value::Null);
            assert_eq!(failure["url"], urls[i]);
            assert!(!failure["reason"].as_str().unwrap().is_empty());
            assert!(!failure["error"].as_str().unwrap().is_empty());
        }
        assert_eq!(
            failed[2]["reason"],
            "No skill directory found in downloaded zip"
        );
        for dir in [".agents", ".claude", ".codex", ".opencode"] {
            assert_eq!(
                fs::read_to_string(ws.join(dir).join("skills/good/SKILL.md"))
                    .await
                    .unwrap(),
                "good skill"
            );
            assert_eq!(
                fs::read_to_string(ws.join(dir).join("skills/direct/SKILL.md"))
                    .await
                    .unwrap(),
                "direct skill"
            );
        }
        assert!(!tmp.path().join("outside").exists());
    }
}

#[tokio::test]
async fn skill_url_failures_all_failed_are_success_and_upload_failure_remains_an_error() {
    let server = downloads().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = state(tmp.path());
    fs::create_dir_all(state.config.project_source_dir.join("p1"))
        .await
        .unwrap();
    let urls = vec![
        format!("{}/missing.zip", server.base),
        format!("{}/broken.zip", server.base),
    ];
    let (status, result) = request(
        state.clone(),
        "/api/project/push-skills-to-workspace",
        &fields(true, urls, false),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["success"], true);
    assert_eq!(result["failedSkills"].as_array().unwrap().len(), 2);
    assert_eq!(result["updatedSkills"], serde_json::json!([]));
    let (status, result) = request(
        state,
        "/api/project/push-skills-to-workspace",
        &fields(true, Vec::new(), false),
        Some(b"corrupt upload".to_vec()),
    )
    .await;
    assert_ne!(status, StatusCode::OK);
    assert_eq!(result["success"], false);
}

#[tokio::test]
async fn skill_url_failures_agent_store_dynamic_push_keeps_failed_destination_and_continues() {
    let server = downloads().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = state(tmp.path());
    let user_root = state.config.computer_workspace_dir.join("u1");
    let ws = user_root.join("c1");
    crate::service::computer_ws::create_workspace_with_agent_store(
        crate::service::computer_ws::CreateAgentStoreParams {
            user_root: &user_root,
            session_workspace: &ws,
            agent_id: "agent1",
            skill_zip: None,
            skill_urls: vec![],
            skill_url_map: None,
            skill_names: None,
            update_skill_names: None,
            hook_config: None,
            downloader: Some(&state.skill_downloader),
            shared_project_id: None,
        },
    )
    .await
    .unwrap();
    let store = crate::service::agent_store::agent_store_path(&user_root, "agent1")
        .unwrap()
        .join("skills");
    fs::write(store.join("blocked"), "existing data")
        .await
        .unwrap();
    let urls = ["blocked.zip", "missing.zip", "good.zip"]
        .map(|name| format!("{}/{name}", server.base))
        .to_vec();
    let (status, result) = request(
        state,
        "/api/computer/push-skills-to-workspace-v2",
        &fields(false, urls, true),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["failedSkills"].as_array().unwrap().len(), 2);
    assert_eq!(result["updatedSkills"], serde_json::json!(["good"]));
    assert_eq!(
        fs::read_to_string(store.join("blocked")).await.unwrap(),
        "existing data"
    );
    assert!(store.join("good/.dynamic_add.lock").exists());
    assert_eq!(
        fs::read_to_string(ws.join(".agents/skills/good/SKILL.md"))
            .await
            .unwrap(),
        "good skill"
    );
}

#[tokio::test]
async fn skill_url_failures_create_workspace_http_legacy_and_named_store_keep_chat_workspace_ready()
{
    let server = downloads().await;
    for agent in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let urls = ["missing.zip", "empty.zip", "good.zip"]
            .map(|name| format!("{}/{name}", server.base))
            .to_vec();
        let mut values = fields(false, urls.clone(), agent);
        if agent {
            values.push((
                "skillUrlMap",
                serde_json::json!({"bad":urls[0],"empty":urls[1],"good":urls[2]}).to_string(),
            ));
        }
        let ws = state.config.computer_workspace_dir.join("u1/c1");
        let (status, result) =
            request(state, "/api/computer/create-workspace-v2", &values, None).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["success"], true);
        let failures = result["failedSkills"].as_array().unwrap();
        assert_eq!(failures.len(), 2);
        if agent {
            assert_eq!(failures[0]["name"], "bad");
            assert_eq!(failures[1]["name"], "empty");
        } else {
            assert_eq!(failures[0]["name"], serde_json::Value::Null);
        }
        assert_eq!(result["updatedSkills"], serde_json::json!(["good"]));
        assert_eq!(
            fs::read_to_string(ws.join(".agents/skills/good/SKILL.md"))
                .await
                .unwrap(),
            "good skill"
        );
        assert!(ws.join(".agents/agents").is_dir());
    }
}
