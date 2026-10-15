//! 正式 start-dev route → 实际 coordinator/executor → 两管真实进程输出。
#[cfg(unix)]
#[tokio::test]
async fn coordinated_dev_typecheck_real_process_retains_business_400_and_unavailable_500() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use tower::ServiceExt as _;
    const CHILD_ROOT: &str = "RCODER_TEST_COORD_TYPECHECK_ROOT";
    const TEST: &str = "preview_assembly::dev_typecheck_tests::coordinated_dev_typecheck_real_process_retains_business_400_and_unavailable_500";
    let Some(root) = std::env::var_os(CHILD_ROOT).map(PathBuf::from) else {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let tools = root.join("tools");
        let project = root.join("projects/p1");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
        std::fs::write(
            project.join("package.json"),
            r#"{"scripts":{"dev":"vite","typecheck":"custom-check"}}"#,
        )
        .unwrap();
        let pnpm = "#!/bin/sh\ncase \"$1\" in\n--version) echo 12.0.0; exit 0;;\ndlx) exit 0;;\ninstall) echo '{\"name\":\"pnpm:stats\",\"added\":0}'; exit 0;;\nrun) if [ -f environment-only ]; then echo missing-tool-task >&2; exit 2; else echo 'src/App.ts(1,2): error TS2322: original stdout detail'; echo 'original stderr detail' >&2; exit 2; fi;;\nesac\nexit 0\n";
        for path in [tools.join("pnpm"), project.join("node_modules/.bin/pnpm")] {
            std::fs::write(&path, pnpm).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let npx = tools.join("npx");
        std::fs::write(&npx, "#!/bin/sh\necho dev-spawned > actual-dev-spawned\necho actual-dev-environment-unavailable >&2\nexit 3\n").unwrap();
        std::fs::set_permissions(npx, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![tools];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(35),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD_ROOT, root)
                .env("PATH", std::env::join_paths(paths).unwrap())
                .env_remove("DEV_SERVER_OVERRIDE_CMD")
                // Isolate the child from an enclosing managed command scope.
                .env_remove("RCODER_COMMAND_WORK_ROOT")
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .contains("COORDINATED_TSC_HTTP_400_AND_UNAVAILABLE_500_CONFIRMED"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    fn state(root: &Path) -> file_server::AppState {
        let config = Arc::new(file_server::Config {
            project_source_dir: root.join("projects"),
            computer_workspace_dir: root.join("computer"),
            log_base_dir: root.join("logs"),
            upload_project_dir: root.join("uploads"),
            dist_target_dir: root.join("dist"),
            dev_command_timeout_secs: 5,
            dev_alive_max_wait_ms: 1_000,
            dev_alive_poll_interval_ms: 20,
            ..Default::default()
        });
        let manager = Arc::new(file_server::DevServerManager::new(config.clone()));
        let coordinator = Arc::new(preview_coordinator::PreviewCoordinator::new(
            Arc::new(preview_coordinator::InProcessPreviewStore::new()),
            Arc::new(file_server::service::dev_server::DevServerExecutor::new(
                manager.clone(),
            )),
            Arc::new(preview_coordinator::SingleInstanceEvidence),
            "fixture-token".into(),
            preview_coordinator::CoordinatorConfig {
                start_budget_secs: 15,
                ..Default::default()
            },
        ));
        file_server::AppState {
            resolver: Arc::new(file_server::LocalWorkspaceResolver::new(
                config.project_source_dir.clone(),
                config.computer_workspace_dir.clone(),
            )),
            build_manager: Arc::new(file_server::BuildManager::new(2)),
            log_cache: Arc::new(file_server::LogCacheManager::new(&config)),
            skill_downloader: Arc::new(file_server::SkillDownloader::new(&config).unwrap()),
            config,
            dev_server: manager,
            preview: Some(coordinator),
            started_at: std::time::Instant::now(),
        }
    }
    let state = state(&root);
    let (router, _) = file_server::routes::api_router().split_for_parts();
    let router = router.with_state(state);
    let response = router
        .clone()
        .oneshot(
            Request::get("/api/build/start-dev?projectId=p1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), 100_000)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "TYPECHECK_FAILED");
    assert_eq!(body["error"]["type"], "BUSINESS_ERROR");
    assert_eq!(body["error"]["details"]["code"], "TYPECHECK_FAILED");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("original stdout detail") && message.contains("original stderr detail"),
        "{body}"
    );
    let project = root.join("projects/p1");
    assert!(
        !project.join("actual-dev-spawned").exists(),
        "TSC rejection starts no dev process"
    );
    tokio::fs::write(
        project.join("environment-only"),
        "current source correction",
    )
    .await
    .unwrap();
    let response = router
        .oneshot(
            Request::get("/api/build/start-dev?projectId=p1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "actual runtime unavailability must remain protected 500"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 100_000)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "UNKNOWN_ERROR");
    assert_eq!(body["error"]["type"], "SYSTEM_ERROR");
    assert!(
        project.join("actual-dev-spawned").exists(),
        "non-TS failure permits the next actual dev attempt through original entry"
    );
    println!("COORDINATED_TSC_HTTP_400_AND_UNAVAILABLE_500_CONFIRMED");
}
