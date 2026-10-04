use super::manifest::{append_managed_launch_env, platform_launch_env};
use super::*;

#[test]
fn managed_launch_context_is_complete_and_scoped_to_the_platform_source() {
    let temp = tempfile::tempdir().unwrap();
    let source = std::fs::canonicalize(temp.path()).unwrap().join("11");
    let state = source.join("state/11");
    std::fs::create_dir_all(&state).unwrap();
    let environment = [
        ("SERVICE_TYPE", std::ffi::OsString::from("user-app-builder")),
        ("APP_CLI_MANAGED", "1".into()),
        ("PROJECT_ID", "11".into()),
        (
            "USERAPP_WORKSPACE_DIR",
            source.parent().unwrap().as_os_str().to_owned(),
        ),
        ("APP_CLI_RUNTIME_WORKSPACE", source.clone().into_os_string()),
        ("APP_CLI_STATE_ROOT", state.clone().into_os_string()),
    ];
    let lookup = |key: &str| {
        environment
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.clone())
    };
    let mut extra = Vec::new();
    platform_launch_env(
        Some(std::ffi::OsStr::new("11")),
        Some(state.as_os_str()),
        &source,
        &mut extra,
    );
    append_managed_launch_env(&source, &state, &mut extra, lookup).unwrap();
    for (key, value) in &environment {
        assert!(
            extra
                .iter()
                .any(|(name, actual)| name == key && std::ffi::OsStr::new(actual) == value)
        );
    }
    assert!(
        append_managed_launch_env(&source.join("foreign"), &state, &mut Vec::new(), lookup)
            .is_err()
    );
    let mut standalone = Vec::new();
    append_managed_launch_env(&source, &state, &mut standalone, |_| None).unwrap();
    assert!(standalone.is_empty());

    let mut stale = vec![
        ("SERVICE_TYPE".into(), "userapp-builder".into()),
        (
            "APP_CLI_RUNTIME_WORKSPACE".into(),
            source.join("code").display().to_string(),
        ),
    ];
    append_managed_launch_env(&source, &state, &mut stale, |key| {
        if key == "APP_CLI_RUNTIME_WORKSPACE" {
            Some(source.join("code").into_os_string())
        } else {
            lookup(key)
        }
    })
    .unwrap();
    for (name, _) in &environment {
        assert_eq!(
            stale.iter().filter(|(key, _)| key == name).count(),
            1,
            "managed child authority must not include duplicate keys: {name}"
        );
    }
    assert!(
        stale
            .iter()
            .any(|(key, value)| key == "APP_CLI_RUNTIME_WORKSPACE"
                && value == &source.display().to_string())
    );
}

// These two environment-injection tests execute a POSIX shell fixture.
#[cfg(all(test, unix))]
mod cases {
    use super::*;
    use std::time::Duration;

    /// 假编排器测试装配：脚本 dump 自身 env 后驻留 60s（宽松就绪——HTTP 探不到
    /// 但进程存活即过，与 vite 路径同语义）。返回 env dump 文件路径。
    fn fake_orchestrator(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let dump = dir.join("env.txt");
        let script = dir.join("fake-app-cli.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\n[ \"$1\" = run ] && [ \"$2\" = --workspace ] || exit 2\nenv | sort > '{}'\nsleep 60\n", dump.display()),
        )
        .expect("write fake orchestrator");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod script");
        }
        (dump, script)
    }

    fn manager_with(script: &Path, logs: &Path) -> DevServerManager {
        let mut config = crate::Config::from_env().expect("test config");
        config.app_cli_bin = Some(script.display().to_string());
        config.log_base_dir = logs.to_path_buf();
        // 探测地址指向必然拒绝连接的端口：spawn 前探测恒 fallthrough，
        // 避免与 owner_probe_branches（占用 3010）跨测试进程竞争
        config.app_cli_admin_probe_addr = "127.0.0.1:1".to_string();
        // 快速宽松就绪（假编排器不监听 9080，走"进程存活"分支）
        config.dev_alive_max_wait_ms = 300;
        config.dev_alive_check_timeout_ms = 100;
        config.dev_alive_poll_interval_ms = 50;
        DevServerManager::new(Arc::new(config))
    }

    /// 等待假编排器落盘 env dump（spawn 异步于断言）。
    async fn wait_dump(dump: &Path) -> String {
        for _ in 0..200 {
            if let Ok(txt) = std::fs::read_to_string(dump)
                && !txt.is_empty()
            {
                return txt;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("fake orchestrator env dump not written: {}", dump.display());
    }

    /// R1（DEV-1 Fix A 事故锚点 app 211）：平台身份指向本项目时
    /// PROJECT_ID/APP_CLI_STATE_ROOT 必须透传编排子进程（原 env_clear 白名单
    /// 缺这两键 → 子进程 standalone registry 段与父进程平台根分岔）。
    /// 纯函数反例覆盖三种身份组合；接线用默认进程 env（无平台变量）验证
    /// standalone 不注入。
    #[test]
    fn platform_launch_env_decides_passthrough_by_identity() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("211");
        std::fs::create_dir_all(&ws).unwrap();
        let state_root = dir.path().join("state").join("211");

        // 匹配：两键透传 + 根返回。
        let mut extra = Vec::new();
        let root = platform_launch_env(
            Some(std::ffi::OsStr::new("211")),
            Some(state_root.as_os_str()),
            &ws,
            &mut extra,
        );
        assert_eq!(root.as_deref(), Some(state_root.as_path()));
        assert!(extra.contains(&("PROJECT_ID".into(), "211".into())));
        assert!(extra.contains(&(
            "APP_CLI_STATE_ROOT".into(),
            state_root.display().to_string()
        )));

        // 不匹配：完全不注入（standalone 语义）。
        let mut extra = Vec::new();
        let root = platform_launch_env(
            Some(std::ffi::OsStr::new("other-app")),
            Some(state_root.as_os_str()),
            &ws,
            &mut extra,
        );
        assert!(root.is_none());
        assert!(extra.is_empty(), "mismatched identity must forward nothing");

        // 无平台变量：standalone。
        let mut extra = Vec::new();
        let root = platform_launch_env(None, None, &ws, &mut extra);
        assert!(root.is_none());
        assert!(extra.is_empty());

        // 匹配但无显式根：仍透传 PROJECT_ID（identity 连续性），根为 None。
        let mut extra = Vec::new();
        let root = platform_launch_env(Some(std::ffi::OsStr::new("211")), None, &ws, &mut extra);
        assert!(root.is_none());
        assert!(extra.contains(&("PROJECT_ID".into(), "211".into())));
    }

    /// R1 接线（真实 spawn）：测试进程无平台变量（standalone）时编排子进程
    /// env 不含 PROJECT_ID——与 platform_launch_env 决策一致的端到端锚点。
    #[tokio::test]
    async fn start_dev_manifest_standalone_env_has_no_platform_identity() {
        if std::env::var_os("PROJECT_ID").is_some() {
            // 宿主环境带平台变量时跳过（避免与决策函数的匹配分支耦合）。
            return;
        }
        let ws = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(ws.path().join("workspace.manifest.toml"), "").expect("marker");
        let dir = tempfile::tempdir().expect("harness tempdir");
        let (dump, script) = fake_orchestrator(dir.path());
        let manager = manager_with(&script, &dir.path().join("logs"));
        manager
            .start_dev(
                "standalone-env-test",
                ws.path(),
                crate::service::dev_server::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: None,
                    request_context: None,
                    artifact_release_id: None,
                },
            )
            .await
            .expect("start manifest dev");
        let env_txt = wait_dump(&dump).await;
        assert!(
            !env_txt.lines().any(|l| l.starts_with("PROJECT_ID=")),
            "standalone spawn must not receive platform identity:\n{env_txt}"
        );
        manager
            .stop_dev("standalone-env-test")
            .await
            .expect("stop dev");
    }

    /// pg 凭据注入：start_dev_manifest(Some) 必须把 POSTGRES_USER/PASSWORD
    /// 写进编排进程 env（覆盖容器默认透传）——save-db-credential 改密后的
    /// 新凭据经此到达服务进程；None 则维持容器 env 透传行为（父进程无该键
    /// 时结果里不出现）。
    #[tokio::test]
    async fn start_dev_manifest_pg_credential_env_injection() {
        let ws = tempfile::tempdir().expect("workspace tempdir");
        // manifest 分流标记（内容不消费——分流只看存在性；假编排器不读它）
        std::fs::write(ws.path().join("workspace.manifest.toml"), "").expect("manifest marker");
        let dir = tempfile::tempdir().expect("harness tempdir");
        let (dump, script) = fake_orchestrator(dir.path());
        let manager = manager_with(&script, &dir.path().join("logs"));

        // Some(pg)：注入覆盖
        let pg = shared_types::StartPgCredential {
            username: "biz_user".into(),
            password: "s3cret".into(),
        };
        let started = manager
            .start_dev(
                "pg-inject-test",
                ws.path(),
                crate::service::dev_server::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: Some(&pg),
                    request_context: None,
                    artifact_release_id: None,
                },
            )
            .await
            .expect("start manifest dev");
        assert_eq!(started.port, shared_types::APP_ENTRY_PORT);
        let env_txt = wait_dump(&dump).await;
        for expect in [
            "POSTGRES_USER=biz_user",
            "POSTGRES_PASSWORD=s3cret",
            "APP_CLI_RUN_PROFILE=dev",
        ] {
            assert!(
                env_txt.lines().any(|l| l == expect),
                "expect {expect} in orchestrator env:\n{env_txt}"
            );
        }
        manager.stop_dev("pg-inject-test").await.expect("stop dev");

        // None：不注入——期望值 = 父进程 env 透传结果（无则不出现）
        std::fs::remove_file(&dump).expect("reset dump");
        let started = manager
            .start_dev(
                "pg-inject-none-test",
                ws.path(),
                crate::service::dev_server::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: None,
                    request_context: None,
                    artifact_release_id: None,
                },
            )
            .await
            .expect("start manifest dev (no pg)");
        assert_eq!(started.port, shared_types::APP_ENTRY_PORT);
        let env_txt = wait_dump(&dump).await;
        for (key, injected) in [
            ("POSTGRES_USER", "biz_user"),
            ("POSTGRES_PASSWORD", "s3cret"),
        ] {
            let lines: Vec<String> = env_txt
                .lines()
                .filter(|l| l.starts_with(&format!("{key}=")))
                .map(|l| l.to_string())
                .collect();
            match std::env::var(key) {
                Ok(parent) => {
                    // 白名单透传：值 = 父进程值（绝不能是注入值）
                    assert_eq!(lines, vec![format!("{key}={parent}")], "passthrough {key}");
                }
                Err(_) => assert!(lines.is_empty(), "{key} 不得凭空出现: {lines:?}"),
            }
            assert!(
                !lines.iter().any(|l| l == &format!("{key}={injected}")),
                "None 时不得注入 {key}"
            );
        }
        manager
            .stop_dev("pg-inject-none-test")
            .await
            .expect("stop dev");
    }
}

#[cfg(test)]
mod owner_reuse_tests {
    use super::*;
    use crate::Config;
    use std::sync::Arc;

    /// P3-02 探测分支（串行四阶段——nextest 每测试独立进程，3010 需独占）：
    /// ① 无监听 → fallthrough spawn；② legacy 应答 → 拒绝；
    /// ③ 匹配 owner + token → 经运行 API 复用并登记 external；
    /// ④ foreign workspace → 拒绝。
    #[tokio::test]
    async fn owner_probe_branches() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::from_env().expect("test config");
        config.log_base_dir = dir.path().join("logs");
        let mgr = DevServerManager::new(Arc::new(config));

        // ① 无监听（此阶段 3010 必须空闲——串行前提）
        let ws_fresh = dir.path().join("ws-fresh");
        std::fs::create_dir_all(&ws_fresh).unwrap();
        assert!(
            mgr.reuse_or_refuse_owner("userapp:app", &ws_fresh, None, None, None, None)
                .await
                .expect("probe")
                .is_none(),
            "no responder must fall through to spawn"
        );

        // ② legacy app-cli 应答（仅 /v1/deploy/status，真实 wire 信封：
        // HttpResult 外壳 + data 内嵌 protocol_version——R2 事故锚点：顶层
        // 裸值曾被误当契约）
        #[derive(serde::Serialize)]
        struct LegacyDeployStatus {
            protocol_version: u32,
            phase: &'static str,
        }
        let legacy = axum::Router::new().route(
            "/v1/deploy/status",
            axum::routing::get(|| async {
                axum::Json(shared_types::HttpResult::success(LegacyDeployStatus {
                    protocol_version: 4,
                    phase: "running",
                }))
            }),
        );
        let legacy_mock = serve_mock(legacy).await;
        let ws_legacy = dir.path().join("ws-legacy");
        std::fs::create_dir_all(&ws_legacy).unwrap();
        let error = mgr
            .reuse_or_refuse_owner("userapp:app", &ws_legacy, None, None, None, None)
            .await
            .expect_err("legacy responder must be refused");
        let AppError::Business(message) = &error else {
            panic!("expected business error, got {error:?}")
        };
        assert!(
            message.contains("legacy app-cli"),
            "diagnostic should name legacy: {message}"
        );

        legacy_mock.release().await;

        // ③ 匹配 owner + token 文件 → 复用
        let ws_reuse = dir.path().join("ws-reuse");
        std::fs::create_dir_all(&ws_reuse).unwrap();
        let state_root = dir.path().join(".app-cli-state").join("unknown-app");
        std::fs::create_dir_all(&state_root).unwrap();
        std::fs::write(state_root.join("token"), "test-token").unwrap();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner = mock_owner_router("hashed-workspace", &ws_reuse).with_state(polls);
        let owner_mock = serve_mock(owner).await;
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();
        let hooks = crate::service::dev_server::supervise::DevEventHooks {
            on_line: std::sync::Arc::new(move |json: &str| {
                sink.lock().unwrap().push(json.to_string());
            }) as process::OnLineCallback,
            on_end: None,
        };
        let started = mgr
            .reuse_or_refuse_owner("userapp:app", &ws_reuse, Some(hooks), None, None, None)
            .await
            .expect("reuse path")
            .expect("must reuse existing owner");
        assert_eq!(started.port, shared_types::APP_ENTRY_PORT);
        // R06：事件按游标重放转发为旧 EVT 形态——服务级事件透传、纯 stage
        // 记录跳过、终态 Completed 映射为平台 orchestration_done（真实 owner
        // 成功也产生 Done 终局）
        let (got_done, got_service) = {
            let events = received.lock().unwrap();
            (
                events.iter().any(|json| {
                    json.contains("\"event\":\"orchestration_done\"")
                        && json.contains("\"failed\":[]")
                }),
                events.iter().any(|json| {
                    json.contains("\"event\":\"service_starting\"") && json.contains("\"frontend\"")
                }),
            )
        };
        assert!(
            got_done,
            "terminal Completed must map to orchestration_done with empty failed; got {:?}",
            received.lock().unwrap()
        );
        assert!(
            got_service,
            "service events must pass through; got {:?}",
            received.lock().unwrap()
        );
        {
            let registered = lock(&mgr.processes).unwrap();
            let entry = registered.get("userapp:app").expect("registered");
            assert_eq!(
                entry.external_owner.as_ref().expect("external").token,
                "test-token"
            );
        }

        owner_mock.release().await;

        // ④ foreign workspace → 拒绝
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let foreign = mock_owner_router("ws-someone-else", &dir.path().join("someone-else"))
            .with_state(polls);
        let _foreign_mock = serve_mock(foreign).await;
        let ws_mine = dir.path().join("ws-mine");
        std::fs::create_dir_all(&ws_mine).unwrap();
        let error = mgr
            .reuse_or_refuse_owner("userapp:app", &ws_mine, None, None, None, None)
            .await
            .expect_err("foreign owner must be refused");
        let AppError::Business(message) = &error else {
            panic!("expected business error, got {error:?}")
        };
        assert!(
            message.contains("different app-cli owner"),
            "diagnostic should name the conflict: {message}"
        );
    }

    #[tokio::test]
    async fn explicit_restart_attaches_verified_owner_without_prior_registration_or_stop() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let root = dir.path().join(".app-cli-state/unknown-app");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("token"), "test-token").unwrap();
        let requests = Arc::new(Mutex::new(
            Vec::<shared_types::RuntimeOperationRequest>::new(),
        ));
        let captured = requests.clone();
        let router = mock_owner_router("hashed-workspace", &workspace)
            .with_state(Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let captured = captured.clone();
                    async move {
                        if request.method() == axum::http::Method::POST {
                            assert_eq!(request.headers()["x-deploy-token"], "test-token");
                            let (parts, body) = request.into_parts();
                            let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
                            captured
                                .lock()
                                .unwrap()
                                .push(serde_json::from_slice(&bytes).unwrap());
                            next.run(axum::extract::Request::from_parts(
                                parts,
                                axum::body::Body::from(bytes),
                            ))
                            .await
                        } else {
                            next.run(request).await
                        }
                    }
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::from_env().unwrap();
        config.log_base_dir = dir.path().join("logs");
        config.app_cli_admin_probe_addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let manager = DevServerManager::new(Arc::new(config));
        assert!(manager.list_dev().unwrap().is_empty());
        manager
            .restart_dev(
                "userapp:123",
                &workspace,
                None,
                None,
                None,
                Some("new-explicit-task"),
            )
            .await
            .unwrap();
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                1,
                "Restart must not issue a preliminary Stop"
            );
            assert_eq!(
                requests[0].kind,
                shared_types::RuntimeOperationKind::Restart
            );
            assert_eq!(requests[0].expected_runtime_instance_id, "instance-test");
            assert_eq!(
                requests[0].request_context.as_deref(),
                Some("new-explicit-task")
            );
            assert_eq!(requests[0].expected_revision, 0);
        }
        // A different canonical project must not use the same authenticated control endpoint.
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert!(
            manager
                .restart_dev(
                    "userapp:456",
                    &other,
                    None,
                    None,
                    None,
                    Some("foreign-task")
                )
                .await
                .is_err()
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        server.abort();
    }

    /// DEV-R1（正常 serve 复用不被磁盘 phase 挡住）：同项目 serve owner 存活
    ///（identity/status/operations 全 wire），磁盘监督记录 phase=ready——
    /// 复用分支先于本地 run 分类执行：start_dev 返回复用结果（提交
    /// Restart），不以"still running"拒绝（旧序在此 phase 直接拒绝 Start）。
    #[tokio::test]
    async fn ready_serve_owner_is_reused_despite_ready_supervision_record() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws-serve-reuse");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("workspace.manifest.toml"), "# t\n").unwrap();
        // registry 登记 + phase=ready 的监督记录（发现侧会将其视为活 run 目标）。
        let state_root = dir.path().join(".app-cli-state").join("unknown-app");
        std::fs::create_dir_all(&state_root).unwrap();
        std::fs::write(state_root.join("token"), "test-token").unwrap();
        let canonical = std::fs::canonicalize(&workspace).unwrap();
        let registry = std::collections::BTreeMap::from([(
            canonical.to_string_lossy().into_owned(),
            "unknown-app".to_string(),
        )]);
        std::fs::write(
            dir.path().join(".app-cli-state/registry.json"),
            serde_json::to_string(&registry).unwrap(),
        )
        .unwrap();
        let discovery = crate::service::dev_server::supervision_tests::WireDiscoveryFile {
            version: 2,
            instance: "serve-supervisor".into(),
            address: "127.0.0.1:1".into(),
            token: "fixture-token".into(),
            snapshot: runtime_supervisor::Snapshot {
                version: 1,
                binding: runtime_supervisor::Binding {
                    component: "app-cli".into(),
                    resource: canonical,
                },
                supervisor_id: "serve-supervisor".into(),
                generation: None,
                phase: runtime_supervisor::Phase::Ready,
                intent: runtime_supervisor::Intent::Run,
                operation_id: None,
                error: None,
                problem: None,
            },
            requests: Vec::new(),
        };
        std::fs::write(
            state_root.join("supervisor.json"),
            serde_json::to_vec(&discovery).unwrap(),
        )
        .unwrap();

        let requests = Arc::new(Mutex::new(
            Vec::<shared_types::RuntimeOperationRequest>::new(),
        ));
        let captured = requests.clone();
        let router = mock_owner_router("hashed-workspace", &workspace)
            .with_state(Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let captured = captured.clone();
                    async move {
                        if request.method() == axum::http::Method::POST {
                            let (parts, body) = request.into_parts();
                            let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
                            captured
                                .lock()
                                .unwrap()
                                .push(serde_json::from_slice(&bytes).unwrap());
                            next.run(axum::extract::Request::from_parts(
                                parts,
                                axum::body::Body::from(bytes),
                            ))
                            .await
                        } else {
                            next.run(request).await
                        }
                    }
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::from_env().unwrap();
        config.log_base_dir = dir.path().join("logs");
        config.app_cli_admin_probe_addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let manager = DevServerManager::new(Arc::new(config));

        let started = manager
            .start_dev(
                "userapp:serve-reuse",
                &workspace,
                crate::service::dev_server::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: None,
                    request_context: None,
                    artifact_release_id: None,
                },
            )
            .await
            .expect("a ready serve owner must be reused, not refused by the disk phase");
        // 复用结果形态（owner 进程保留，pid 0 + 固定入口端口）。
        assert_eq!(started.pid, 0);
        assert_eq!(started.port, shared_types::APP_ENTRY_PORT);
        let posted = requests.lock().unwrap();
        assert_eq!(
            posted.len(),
            1,
            "exactly the reuse Restart submission: {posted:?}"
        );
        assert_eq!(posted[0].kind, shared_types::RuntimeOperationKind::Restart);
        server.abort();
    }

    /// HttpResult 信封（wire 层格式手拼 OK；data 载荷一律 typed 构造——
    /// 契约结构体字段偏移时编译器强制 mock 同步，不靠字符串检查）。
    fn envelope<T: serde::Serialize>(data: &T) -> serde_json::Value {
        serde_json::json!({"success": true, "code": "OK", "data": data, "message": "ok"})
    }

    fn operation_view(
        id: &str,
        state: shared_types::RuntimeOperationState,
    ) -> shared_types::RuntimeOperationView {
        shared_types::RuntimeOperationView {
            operation_id: id.to_string(),
            kind: shared_types::RuntimeOperationKind::Restart,
            state,
            request_digest: "digest".to_string(),
            revision: 1,
            runtime_instance_id: "instance-test".to_string(),
            error_code: None,
            error_message: None,
            failure_detail: None,
        }
    }

    fn mock_owner_router(
        ws: &str,
        root: &Path,
    ) -> axum::Router<std::sync::Arc<std::sync::atomic::AtomicUsize>> {
        use shared_types::{
            DesiredState, ObservedHealth, RuntimeEventRecord, RuntimeIdentityView,
            RuntimeOperationState, RuntimeStatusView,
        };

        let source_root = root.to_string_lossy().into_owned();
        let identity_of = move |workspace: String| RuntimeIdentityView {
            application_id: "unknown-app".to_string(),
            service_family: "userapp-dev".to_string(),
            workspace_id: workspace,
            source_root: source_root.clone(),
            runtime_instance_id: "instance-test".to_string(),
            deployment_generation_id: "gen-test".to_string(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        let status_view = || RuntimeStatusView {
            desired: DesiredState::Running,
            observed: ObservedHealth::Ready,
            active_target: None,
            revision: 0,
            active_operation_id: None,
            recovery_protection: false,
            runtime_instance_id: "instance-test".to_string(),
        };

        let ws = ws.to_string();
        let identity_ws = ws.clone();
        axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = identity_of(identity_ws.clone());
                    async move { axum::Json(envelope(&identity)) }
                }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(move || async move { axum::Json(envelope(&status_view())) }),
            )
            .route(
                "/v1/runtime/operations",
                axum::routing::post(
                    |axum::extract::State(polls): axum::extract::State<
                        Arc<std::sync::atomic::AtomicUsize>,
                    >,
                     axum::Json(req): axum::Json<serde_json::Value>| async move {
                        assert_eq!(req["workspace_id"], "hashed-workspace");
                        polls.store(1, std::sync::atomic::Ordering::SeqCst);
                        let id = req
                            .get("operation_id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("op")
                            .to_string();
                        let receipt = shared_types::RuntimeOperationAccepted {
                            operation_id: id.clone(),
                            state: RuntimeOperationState::Accepted,
                            poll: format!("/v1/runtime/operations/{id}"),
                        };
                        (
                            axum::http::StatusCode::ACCEPTED,
                            axum::Json(envelope(&receipt)),
                        )
                    },
                ),
            )
            .route(
                "/v1/runtime/operations/{id}",
                axum::routing::get(
                    |axum::extract::Path(id): axum::extract::Path<String>,
                     axum::extract::State(polls): axum::extract::State<
                        Arc<std::sync::atomic::AtomicUsize>,
                    >| async move {
                        if polls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                            return (
                                axum::http::StatusCode::NOT_FOUND,
                                axum::Json(serde_json::json!({"message": "not found"})),
                            );
                        }
                        // 首查 accepted（留事件流消费窗口），其后 succeeded
                        let seen = polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let state = if seen == 1 {
                            RuntimeOperationState::Accepted
                        } else {
                            RuntimeOperationState::Succeeded
                        };
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(envelope(&operation_view(&id, state))),
                        )
                    },
                ),
            )
            .route(
                "/v1/runtime/operations/{id}/events",
                axum::routing::get(
                    |axum::extract::Path(id): axum::extract::Path<String>,
                     axum::extract::Query(query): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        // typed 构造事件记录（R06 重放端点契约：after_seq 游标）：
                        // 服务级事件透传 + 无 event_name 的纯 stage（适配层跳过）
                        // + 终态 Completed（映射为平台 orchestration_done）
                        let after: u64 = query
                            .get("after_seq")
                            .and_then(|value| value.parse().ok())
                            .unwrap_or(0);
                        let all = [
                            RuntimeEventRecord {
                                operation_id: id.clone(),
                                sequence: 1,
                                runtime_instance_id: "instance-test".to_string(),
                                stage: "service".to_string(),
                                service: Some("frontend".to_string()),
                                event_name: Some("service_starting".to_string()),
                                payload: None,
                            },
                            RuntimeEventRecord {
                                operation_id: id.clone(),
                                sequence: 2,
                                runtime_instance_id: "instance-test".to_string(),
                                stage: "stage-only".to_string(),
                                service: None,
                                event_name: None,
                                payload: None,
                            },
                            RuntimeEventRecord {
                                operation_id: id.clone(),
                                sequence: 3,
                                runtime_instance_id: "instance-test".to_string(),
                                stage: "terminal".to_string(),
                                service: None,
                                event_name: Some("Completed".to_string()),
                                payload: None,
                            },
                        ];
                        let events: Vec<_> = all
                            .into_iter()
                            .filter(|record| record.sequence > after)
                            .collect();
                        axum::Json(envelope(&serde_json::json!({
                            "operation_id": id,
                            "events": events,
                        })))
                    },
                ),
            )
    }

    /// R07/R08 反例：构建期观察失败 → 迟到提交拒绝（不刷新期望绕过停止
    /// 屏障）；携带 pg 的复用提交在 wire 上包含 run_config.pg。
    /// 独立自由端口 + 可配置 probe addr（不与 owner_probe_branches 的 3010 竞争）。
    #[tokio::test]
    async fn observation_failure_blocks_late_submission_and_pg_travels_on_wire() {
        use crate::Config;
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws-r78");
        std::fs::create_dir_all(&ws).unwrap();
        let state_root = dir.path().join(".app-cli-state").join("unknown-app");
        std::fs::create_dir_all(&state_root).unwrap();
        // 刻意**先不写 token**：构建期 capture 时 owner 在但凭据缺 →
        // ObservationFailed（真实分类路径，非手工注入）
        let submitted: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = submitted.clone();
        let identity = shared_types::RuntimeIdentityView {
            application_id: "unknown-app".to_string(),
            service_family: "userapp-dev".to_string(),
            workspace_id: "ws-r78".to_string(),
            source_root: ws.to_string_lossy().into_owned(),
            runtime_instance_id: "instance-r78".to_string(),
            deployment_generation_id: "gen".to_string(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        let status_view = shared_types::RuntimeStatusView {
            desired: shared_types::DesiredState::Running,
            observed: shared_types::ObservedHealth::Ready,
            active_target: None,
            revision: 7,
            active_operation_id: None,
            recovery_protection: false,
            runtime_instance_id: "instance-r78".to_string(),
        };
        let app = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = identity.clone();
                    async move { axum::Json(envelope(&identity)) }
                }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(move || async move { axum::Json(envelope(&status_view)) }),
            )
            .route(
                "/v1/runtime/operations",
                axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                    let sink = sink.clone();
                    async move {
                        sink.lock().unwrap().push(body.0.clone());
                        axum::Json(envelope(&serde_json::json!({
                            "operation_id": "op-x",
                            "state": "accepted",
                        })))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let probe_addr = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serve");
        });

        let mut config = Config::from_env().expect("test config");
        config.log_base_dir = dir.path().join("logs");
        config.app_cli_admin_probe_addr = probe_addr.clone();
        let mgr = DevServerManager::new(Arc::new(config));

        // ── R07：构建期 owner 在但凭据缺 → ObservationFailed 分类
        mgr.capture_owner_expectation("userapp:r78", &ws).await;
        {
            let map = lock(&mgr.owner_expectations).unwrap();
            match map.get("userapp:r78") {
                Some(crate::service::dev_server::types::OwnerExpectation::ObservationFailed {
                    ..
                }) => {}
                other => panic!("expected ObservationFailed, got {other:?}"),
            }
        }
        // 网络恢复（补 token）+ 用户 Stop 已推进 revision（mock revision=7）：
        // 旧构建迟到提交必须被拒——不得刷新期望
        std::fs::write(state_root.join("token"), "test-token").unwrap();
        let error = mgr
            .reuse_or_refuse_owner("userapp:r78", &ws, None, None, None, None)
            .await
            .expect_err("late build submission must be refused");
        assert!(
            error.to_string().contains("observation failed"),
            "diagnostic: {error}"
        );
        assert!(
            submitted.lock().unwrap().is_empty(),
            "no operation may be submitted when the admission context is unverified"
        );

        // ── R07 对照：确认无 owner（无人监听端口）→ NoOwner 分类
        let mut config = Config::from_env().expect("test config");
        config.log_base_dir = dir.path().join("other-logs");
        config.app_cli_admin_probe_addr = "127.0.0.1:1".to_string();
        let mgr2 = DevServerManager::new(Arc::new(config));
        mgr2.capture_owner_expectation("userapp:none", &ws).await;
        {
            let map = lock(&mgr2.owner_expectations).unwrap();
            assert!(matches!(
                map.get("userapp:none"),
                Some(crate::service::dev_server::types::OwnerExpectation::NoOwner)
            ));
        }

        // ── R08：清除失败期望（模拟下一轮构建成功捕获后），复用提交在
        // wire 上携带 run_config.pg（新凭据到达 owner）
        lock(&mgr.owner_expectations).unwrap().remove("userapp:r78");
        let pg = shared_types::StartPgCredential {
            username: "biz_user".to_string(),
            password: "new-s3cret".to_string(),
        };
        // 复用路径会 wait_terminal 轮询到超时——mock 不提供 operation 查询端点，
        // 这里只需断言**提交 wire**；超时错误可忽略（操作已受理记录在案）
        let _reuse_result = mgr
            .reuse_or_refuse_owner("userapp:r78", &ws, None, Some(&pg), None, None)
            .await;
        let posted = submitted.lock().unwrap();
        let last = posted
            .last()
            .cloned()
            .unwrap_or_else(|| panic!("restart must be submitted; got {:?}", posted));
        assert_eq!(
            last["run_config"]["pg"]["username"], "biz_user",
            "wire must carry the new pg credential: {last}"
        );
        assert_eq!(last["run_config"]["pg"]["password"], "new-s3cret");
        task.abort();
    }

    /// R03 反例：制品态 owner 路由——owner 拒绝（revision 推进）时 `.run`
    /// 保持原样（提交拒绝不改变 active 内容）；owner 受理时请求按
    /// ArtifactId 形态上 wire（不是 Source restart）。
    #[tokio::test]
    async fn artifact_owner_route_preserves_run_dir_on_rejection() {
        use crate::Config;
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws-art");
        std::fs::create_dir_all(&ws).unwrap();
        // 既有 .run（旧版本运行目录）+ 标记文件——必须原样保留
        std::fs::create_dir_all(ws.join(".run")).unwrap();
        std::fs::write(ws.join(".run").join("marker-old"), "v1").unwrap();
        std::fs::create_dir_all(ws.join("builds")).unwrap();
        let write_candidate = |explicit: bool| {
            let mut lock = shared_types::load_release_lock(include_str!(
                "../../../../../workspace-manifest/tests/fixtures/lock_v1.toml"
            ))
            .unwrap();
            if explicit {
                lock.services[0].health.startup_probe = Some(shared_types::StartupProbe::Http);
            }
            let file =
                std::fs::File::create(ws.join("builds/workspace-package-rel-new.zip")).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file(
                "release.lock.toml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(toml::to_string(&lock).unwrap().as_bytes())
                .unwrap();
            zip.finish().unwrap();
        };
        write_candidate(false);

        // mock owner：workspace_id=".run"（产物态绑定）、token 文件、
        // 受理后固定返回 ERR_REVISION_MISMATCH 拒绝
        let state_root = dir.path().join(".app-cli-state").join("unknown-app");
        std::fs::create_dir_all(&state_root).unwrap();
        std::fs::write(state_root.join("token"), "test-token").unwrap();
        let submitted: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = submitted.clone();
        let identity = shared_types::RuntimeIdentityView {
            application_id: "unknown-app".to_string(),
            service_family: "userapp-dev".to_string(),
            workspace_id: ".run".to_string(),
            source_root: ws.join(".run").to_string_lossy().into_owned(),
            runtime_instance_id: "instance-art".to_string(),
            deployment_generation_id: "gen".to_string(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        let identity_ws = identity.clone();
        let app = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = identity_ws.clone();
                    async move { axum::Json(envelope(&identity)) }
                }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(|| async {
                    let status = shared_types::RuntimeStatusView {
                        desired: shared_types::DesiredState::Running,
                        observed: shared_types::ObservedHealth::Ready,
                        active_target: None,
                        revision: 9,
                        active_operation_id: None,
                        recovery_protection: false,
                        runtime_instance_id: "instance-art".to_string(),
                    };
                    axum::Json(envelope(&status))
                }),
            )
            .route(
                "/v1/runtime/operations",
                axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                    let sink = sink.clone();
                    async move {
                        sink.lock().unwrap().push(body.0.clone());
                        // 模拟构建期间用户 Stop 推进 revision → 受理拒绝
                        axum::response::Response::builder()
                            .status(409)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(
                                serde_json::json!({
                                    "success": false,
                                    "code": "ERR_REVISION_MISMATCH",
                                    "message": "expected revision 8 does not match current 9",
                                })
                                .to_string(),
                            ))
                            .expect("reject response")
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let mut config = Config::from_env().expect("test config");
        config.log_base_dir = dir.path().join("logs");
        config.app_cli_admin_probe_addr = addr;
        let mgr = DevServerManager::new(Arc::new(config));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serve");
        });

        // owner 在但拒绝（revision 已推进）
        let error = mgr
            .route_artifact_restart("userapp:art", &ws, "rel-new", None, None, None)
            .await
            .expect_err("owner rejection must propagate");
        assert!(
            error.to_string().contains("ERR_REVISION_MISMATCH"),
            "diagnostic: {error}"
        );
        // An explicit probe on a candidate artifact is checked before any POST,
        // against the artifact lock (the serving .run has no such field).
        write_candidate(true);
        let error = mgr
            .route_artifact_restart("userapp:art", &ws, "rel-new", None, None, None)
            .await
            .expect_err("old owner cannot execute explicit startup probes");
        assert!(
            error
                .to_string()
                .contains(shared_types::STARTUP_PROBE_CAPABILITY),
            "{error}"
        );
        assert_eq!(
            submitted.lock().unwrap().len(),
            1,
            "unsupported candidate must not submit another restart"
        );
        // R03 核心：`.run` 原样（提交拒绝不改变 active 内容）
        assert!(
            ws.join(".run").join("marker-old").is_file(),
            "active .run must be untouched on owner rejection"
        );
        assert!(
            !ws.join(".previous").exists(),
            "no rotation may happen on rejection"
        );
        // wire：提交按 ArtifactId 形态（artifact_id 上 wire，非 Source）
        let posted = submitted.lock().unwrap();
        let last = posted
            .last()
            .cloned()
            .unwrap_or_else(|| panic!("{posted:?}"));
        // wire 形态跟随 RunProfileInput 的 serde tag（tag="profile"/
        // content="input" + ArtifactInput 的 tag="source"/content="value"）
        assert_eq!(
            last["kind"], "deploy",
            "the owner only admits Deploy + Artifact"
        );
        assert_eq!(
            last["profile"]["profile"], "artifact",
            "artifact restart must use Artifact profile: {last}"
        );
        assert_eq!(
            last["profile"]["input"]["artifact"]["source"],
            "artifact_id"
        );
        assert_eq!(
            last["profile"]["input"]["artifact"]["value"]["artifact_id"],
            "rel-new"
        );
        drop(last);
        drop(posted);
        task.abort();
    }

    /// 起 mock 在 3010；返回显式释放句柄（顺序关闭 + 等待端口回收）。
    async fn serve_mock(app: axum::Router<()>) -> MockOwner {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:3010")
            .await
            .expect("bind 3010");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .expect("mock serve");
        });
        MockOwner {
            task: Some(task),
            tx: Some(tx),
        }
    }

    struct MockOwner {
        task: Option<tokio::task::JoinHandle<()>>,
        tx: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl MockOwner {
        /// 显式关闭并等待端口回收（下一阶段绑定的前提）。
        async fn release(mut self) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(());
            }
            if let Some(task) = self.task.take() {
                let _joined = task.await;
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while tokio::net::TcpListener::bind("127.0.0.1:3010")
                .await
                .is_err()
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "mock owner did not release port 3010"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    impl Drop for MockOwner {
        fn drop(&mut self) {
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }
}
