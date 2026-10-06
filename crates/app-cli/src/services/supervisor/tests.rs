use super::*;

#[cfg(test)]
mod cases {
    use super::*;
    use workspace_manifest::{DevrunSection, RunSection};

    /// 最小 ServiceSpec（LockedService）：只填启动命令相关字段。
    pub(super) fn spec_with(devrun: Option<Vec<&str>>) -> ServiceSpec {
        ServiceSpec {
            service_id: "frontend".into(),
            name: "Frontend".into(),
            dir: "frontend".into(),
            r#type: workspace_manifest::ProjectType::Node,
            kind: workspace_manifest::ProjectKind::Web,
            enabled: true,
            port: 4578,
            devbuild: None,
            run: RunSection {
                command: vec!["node".into(), "server.js".into()],
                migrate: Vec::new(),
                depends_on: Vec::new(),
                shutdown_timeout_seconds: 30,
            },
            devrun: devrun.map(|command| DevrunSection {
                command: command.into_iter().map(String::from).collect(),
            }),
            static_content_dir: None,
            health: Default::default(),
            proxy: None,
            logs: Vec::new(),
            env: Default::default(),
        }
    }

    #[test]
    fn runtime_url_replaces_credentials_without_losing_database_options() {
        let pg = shared_types::StartPgCredential {
            username: "runtimeuser".into(),
            password: "new@:/#?%40password".into(),
        };
        let value = database_url_with_credentials(
            "postgresql://old:stale@localhost:5433/business?sslmode=disable",
            &pg,
        )
        .unwrap();
        let url = reqwest::Url::parse(&value).unwrap();
        assert_eq!(url.username(), "runtimeuser");
        assert_eq!(url.host_str(), Some("localhost"));
        assert_eq!(url.port(), Some(5433));
        assert_eq!(url.path(), "/business");
        assert_eq!(url.query(), Some("sslmode=disable"));
        assert!(!value.contains("stale"));
        assert!(value.contains("%40"));
        assert!(value.contains("%2540password"));
        assert_eq!(url.fragment(), None);
        let error = database_url_with_credentials("not a url privatepassword", &pg).unwrap_err();
        assert!(!error.to_string().contains("privatepassword"));
    }

    #[test]
    fn managed_credentials_remove_query_overrides_in_actual_service_environment() {
        let mut spec = spec_with(None);
        spec.env.insert("DATABASE_URL".into(),
            "postgresql://old:old@localhost/db?user=old&password=old&user=again&%70assword=again&sslmode=disable&options=-c%20search_path%3Dpublic".into());
        spec.env
            .insert("PGSSLROOTCERT".into(), "/fixture/root.crt".into());
        spec.env
            .insert("PGOPTIONS".into(), "-c application_name=fixture".into());
        let pg = shared_types::StartPgCredential {
            username: "managed".into(),
            password: "managedsecret".into(),
        };
        let environment = service_environment(&spec, Some(&pg)).unwrap();
        let url = reqwest::Url::parse(&environment["DATABASE_URL"]).unwrap();
        assert!(
            !url.query_pairs()
                .any(|(key, _)| key == "user" || key == "password")
        );
        let target = pg_probe_environment(environment).unwrap();
        assert_eq!(target["PGUSER"], "managed");
        assert_eq!(target["PGPASSWORD"], "managedsecret");
        assert_eq!(target["PGSSLROOTCERT"], "/fixture/root.crt");
        assert_eq!(target["PGOPTIONS"], "-c search_path=public");
        spec.env.remove("DATABASE_URL");
        spec.env.insert("DATABASE_URL".into(), String::new());
        let target = pg_probe_environment(service_environment(&spec, Some(&pg)).unwrap()).unwrap();
        assert_eq!(target["PGOPTIONS"], "-c application_name=fixture");
    }

    #[test]
    fn operation_credentials_override_artifact_defaults_and_survive_spec_roundtrip() {
        let pg = resolve_run_pg(Some(shared_types::StartPgCredential {
            username: "runtimeuser".into(),
            password: "runtimepassword".into(),
        }))
        .unwrap()
        .unwrap();
        let mut spec = spec_with(None);
        spec.env
            .insert("POSTGRES_USER".into(), "artifactuser".into());
        spec.env
            .insert("POSTGRES_PASSWORD".into(), "artifactpassword".into());
        spec.env.insert("OTHER_SETTING".into(), "retained".into());
        spec.env.insert(
            "DATABASE_URL".into(),
            "postgresql://artifactuser:artifactpassword@localhost/dev".into(),
        );
        let env = service_environment(&spec, Some(&pg)).unwrap();
        assert_eq!(env["POSTGRES_USER"], "runtimeuser");
        assert_eq!(env["POSTGRES_PASSWORD"], "runtimepassword");
        assert_eq!(env["OTHER_SETTING"], "retained");
        assert_eq!(
            env["DATABASE_URL"],
            "postgresql://runtimeuser:runtimepassword@localhost/dev"
        );
        let file = crate::svc_spec::ServiceSpecFile {
            release_id: "release1".into(),
            service_id: "service1".into(),
            cwd: ".".into(),
            argv: vec!["node".into()],
            env,
            port: Some(4200),
        };
        let decoded: crate::svc_spec::ServiceSpecFile =
            toml::from_str(&toml::to_string(&file).unwrap()).unwrap();
        assert_eq!(decoded.env["POSTGRES_PASSWORD"], "runtimepassword");
        assert!(
            !decoded
                .runtime_env_overrides(Path::new("logs"))
                .contains_key("POSTGRES_PASSWORD")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn migration_process_receives_captured_runtime_credentials() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = spec_with(None);
        spec.env.insert("POSTGRES_USER".into(), "staleuser".into());
        spec.env
            .insert("POSTGRES_PASSWORD".into(), "stalepassword".into());
        let pg = shared_types::StartPgCredential {
            username: "operationuser".into(),
            password: "operationpassword".into(),
        };
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "printf '%s\n%s\n' \"$POSTGRES_USER\" \"$POSTGRES_PASSWORD\" > credentials".into(),
        ];
        run_transient_with_env(
            &argv,
            root.path(),
            &service_environment(&spec, Some(&pg)).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("credentials")).unwrap(),
            "operationuser\noperationpassword\n"
        );
    }

    /// PG 预检门控（e39591126 起为环境策略）：平台声明 APP_CLI_REQUIRE_PG=1
    /// 才探测；缺省不探测——即使服务声明了 migrate（独立 app-cli 不从服务
    /// 清单推断本地数据库，60s 轮询不得阻塞无数据库工作区启动）。
    #[test]
    fn pg_wait_is_gated_on_declared_need() {
        // 平台未声明 → 不探测（含声明 migrate 的服务）
        assert!(!pg_required_by_policy(None));
        assert!(!pg_required_by_policy(Some(std::ffi::OsStr::new(""))));
        assert!(!pg_required_by_policy(Some(std::ffi::OsStr::new("0"))));
        // 平台声明（builder 形态注入）→ 探测
        assert!(pg_required_by_policy(Some(std::ffi::OsStr::new("1"))));
    }

    /// dev 形态 + 有 [devrun] → devrun.command（热加载命令生效）。
    #[test]
    fn dev_profile_prefers_devrun_command() {
        let spec = spec_with(Some(vec!["pnpm", "exec", "vite"]));
        let argv = effective_run_argv(&spec, true);
        assert_eq!(argv, &["pnpm", "exec", "vite"]);
    }

    /// dev 形态但未配 [devrun] → 回落 [run].command（未配置服务的兜底语义）。
    #[test]
    fn dev_profile_falls_back_to_run_without_devrun() {
        let spec = spec_with(None);
        let argv = effective_run_argv(&spec, true);
        assert_eq!(argv, &["node", "server.js"]);
    }

    /// 非 dev 形态（生产/本地直跑）恒走 [run]——即便配置了 [devrun] 也不生效。
    #[test]
    fn prod_profile_always_uses_run_command() {
        let spec = spec_with(Some(vec!["pnpm", "exec", "vite"]));
        let argv = effective_run_argv(&spec, false);
        assert_eq!(argv, &["node", "server.js"]);
    }

    /// 窗口内探测超时：无人监听的端口在 1s 窗口内轮询后 Err（含 URL 与窗口信息）。
    /// （成功分支与 bridge 等待共用同一探测核心，由集成/冒烟覆盖。）
    #[tokio::test]
    async fn readiness_probe_times_out_within_window() {
        // 找一个确定空闲的端口：bind 后立即释放（TIME_WAIT 由 connect 端触发，服务端无）
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let spec = spec_with(None);
        let spec = crate::manifest::ServiceSpec {
            port,
            health: workspace_manifest::HealthSection {
                readiness_path: "/ready".into(),
                ..Default::default()
            },
            ..spec
        };
        let err = wait_for_service_ready_within(&spec, 1)
            .await
            .expect_err("must time out");
        let message = format!("{err:#}");
        assert!(message.contains("not ready within 1 seconds"), "{message}");
        assert!(message.contains("/ready"), "{message}");
    }
    /// R08 反例：owner 复用的每操作 PG 凭据必须到达服务进程 env——
    /// run_with_cancel(pg) → start_service 注入 POSTGRES_USER/PASSWORD
    /// （last-wins 覆盖 spec.env 与进程透传值）。
    #[cfg(unix)]
    #[tokio::test]
    async fn per_operation_pg_credential_reaches_service_env() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let service_dir = workspace.join("web");
        std::fs::create_dir_all(&service_dir).unwrap();
        let log_dir = dir.path().join("logs");
        let dump = dir.path().join("env.dump");
        let spec = ServiceSpec {
            service_id: "web".into(),
            name: "Web".into(),
            dir: "web".into(),
            r#type: workspace_manifest::ProjectType::Node,
            kind: workspace_manifest::ProjectKind::Web,
            enabled: true,
            port: 4578,
            devbuild: None,
            run: RunSection {
                // spec.env 里的旧凭据必须被运行时变量覆盖（last-wins）
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    // 完成标记防撕裂读（env 逐段写文件，POSTGRES_* 按序靠后）
                    format!(
                        "env | sort > '{}'; echo __DUMP_DONE__ >> '{}'; sleep 30",
                        dump.display(),
                        dump.display()
                    ),
                ],
                migrate: Vec::new(),
                depends_on: Vec::new(),
                shutdown_timeout_seconds: 0,
            },
            devrun: None,
            static_content_dir: None,
            health: Default::default(),
            proxy: None,
            logs: Vec::new(),
            env: [
                ("POSTGRES_USER".to_string(), "stale-user".to_string()),
                ("POSTGRES_PASSWORD".to_string(), "stale-pass".to_string()),
            ]
            .into(),
        };
        let pg = shared_types::StartPgCredential {
            username: "biz_user".into(),
            password: "new-s3cret".into(),
        };
        let mut child = start_service(
            &spec,
            &spec.run.command,
            &workspace,
            &log_dir,
            "rel-1",
            Some(&pg),
        )
        .await
        .unwrap();
        // 等 env dump 落盘（spawn 异步）
        let content = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&dump).await
                    && text.contains("__DUMP_DONE__")
                {
                    break text;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            content.lines().any(|l| l == "POSTGRES_USER=biz_user"),
            "new username must win over spec.env; got:\n{content}"
        );
        assert!(
            content.lines().any(|l| l == "POSTGRES_PASSWORD=new-s3cret"),
            "new password must win over spec.env; got:\n{content}"
        );
        let _ = child.stop(Duration::from_secs(5)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forced_shutdown_reaps_real_child_and_confirms_group_absence() {
        use tokio::io::AsyncBufReadExt;
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
            .stdout(Stdio::piped());
        let mut child = spawn_owned(command, None).await.unwrap();
        let pid = child.id().unwrap();
        let mut lines = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("ready"));
        shutdown_all(vec![("ignores-term".into(), child)], 0)
            .await
            .unwrap();
        assert!(!process_utils::process_group_exists(pid).unwrap());
    }

    /// R01 反例：服务根进程退出、孙进程仍存活持组时，监督循环的退出检测必须
    /// 立即触发（root 语义）——树级 try_wait 会把根进程死亡掩盖到孙进程退出，
    /// 服务该重启时不重启。
    #[cfg(unix)]
    #[tokio::test]
    async fn exit_detection_fires_on_root_exit_even_with_live_grandchild() {
        let mut command = Command::new("sh");
        // root 立即退出，孙进程 sleep 60 继续存活持有进程组
        command
            .args(["-c", "sleep 60 & exit 0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_owned(command, None).await.unwrap();
        let pid = child.id().unwrap();
        // 等 root 退出事实落地（孙进程仍在）
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait_root())
            .await
            .expect("root wait deadline")
            .unwrap();
        assert!(status.success());
        assert!(
            process_utils::process_group_exists(pid).unwrap(),
            "grandchild must still be alive to set up the regression fixture"
        );
        // root 语义退出检测必须立刻识别（500ms 轮询周期内）
        let mut children = vec![("root-exited".into(), child)];
        let detected =
            tokio::time::timeout(Duration::from_secs(3), poll_any_exit(&mut children, &[]))
                .await
                .expect("poll_any_exit must fire on root exit while grandchild lives");
        assert_eq!(detected, Some("root-exited".into()));
        shutdown_all(children, 0).await.unwrap();
        assert!(!process_utils::process_group_exists(pid).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn migration_timeout_confirms_original_process_group_stopped() {
        let root = tempfile::tempdir().unwrap();
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "echo $$ > migration.pid; exec sleep 30".into(),
        ];
        let result =
            run_transient_with_timeout(&argv, root.path(), Duration::from_millis(100)).await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
        let pid: u32 = std::fs::read_to_string(root.path().join("migration.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!process_utils::process_group_exists(pid).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_migration_cannot_leave_background_group_writing() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().to_owned();
        let migration = tokio::spawn(async move {
            let argv: Vec<String> = vec![
                "sh".into(),
                "-c".into(),
                "echo $$ > migration.pid; while [ ! -f exit-now ]; do :; done; exit 0".into(),
            ];
            run_transient_with_timeout(&argv, &directory, Duration::from_secs(5)).await
        });
        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(root.path().join("migration.pid")).await
                    && let Ok(pid) = text.trim().parse::<u32>()
                    // 撕裂读出的 pid 前缀几乎必为死组：join 前确认组确实存在
                    && matches!(process_utils::process_group_exists(pid), Ok(true))
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Keep a second real process in the migration group, but parent it to the
        // test so it is reaped deterministically instead of depending on PID 1.
        let mut member = None;
        for _ in 0..3 {
            let mut command = Command::new("sleep");
            command.arg("30").process_group(i32::try_from(pid).unwrap());
            match command.spawn() {
                Ok(child) => {
                    member = Some(child);
                    break;
                }
                // tokio/std spawn 的瞬时 ECHILD（运行时竞态）：短暂退避重试
                Err(error) if error.raw_os_error() == Some(10) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("spawn migration group member: {error}"),
            }
        }
        let mut member = member.expect("member spawn after retries");
        let reaper = tokio::spawn(async move { member.wait().await });
        tokio::fs::write(root.path().join("exit-now"), b"go")
            .await
            .unwrap();
        migration.await.unwrap().unwrap();
        // 成员死亡证据是下方组探测 ESRCH。成员 wait 可能被树收束确认路径的
        // waitpid(-pgid) 抢先 reap（tokio 视角 ECHILD）——那是确认收束的正
        // 常组成，不构成失败；仍可观测到退出码时必须非成功。
        match reaper.await.expect("reaper join") {
            Ok(status) => assert!(!status.success()),
            Err(error) if error.raw_os_error() == Some(10) => {}
            Err(error) => panic!("member wait: {error}"),
        }
        assert!(!process_utils::process_group_exists(pid).unwrap());
    }
}

#[cfg(test)]
mod pg_readiness_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn pg_probe_uses_migration_database_and_managed_credentials() {
        let environment = BTreeMap::from([
            ("DATABASE_URL".into(), "".into()),
            ("PGHOST".into(), "127.0.0.2".into()),
            ("PGPORT".into(), "5439".into()),
            ("POSTGRES_USER".into(), "runtimeuser".into()),
            ("POSTGRES_PASSWORD".into(), "fixturepassword".into()),
            ("POSTGRES_DB".into(), "applicationdb".into()),
        ]);
        let target = pg_probe_environment(environment).unwrap();
        assert_eq!(target["PGDATABASE"], "applicationdb");
        assert_eq!(target["PGUSER"], "runtimeuser");
        assert_eq!(target["PGHOST"], "127.0.0.2");
        assert_eq!(target["PGPORT"], "5439");
        assert_eq!(target["PGPASSWORD"], "fixturepassword");
        let url = "postgresql://fixture:secret@localhost:5440/otherdb?sslmode=disable";
        let target = pg_probe_environment(BTreeMap::from([
            ("DATABASE_URL".into(), url.into()),
            ("POSTGRES_DB".into(), "ignored".into()),
        ]))
        .unwrap();
        assert_eq!(target["PGDATABASE"], "otherdb");
        assert_eq!(target["PGUSER"], "fixture");
        assert_eq!(target["PGPASSWORD"], "secret");
        assert_eq!(target["PGHOST"], "localhost");
        assert_eq!(target["PGPORT"], "5440");
        assert_eq!(target["PGSSLMODE"], "disable");
        let target = pg_probe_environment(BTreeMap::from([("DATABASE_URL".into(),
            "postgresql://user:p%40ss%2Bword@[::1]:5442/db%20name?host=%2Ftmp&dbname=overridden&options=-c%20search_path%3Dpublic".into())])).unwrap();
        assert_eq!(target["PGHOST"], "/tmp");
        assert_eq!(target["PGPASSWORD"], "p@ss+word");
        assert_eq!(target["PGDATABASE"], "overridden");
        assert_eq!(target["PGOPTIONS"], "-c search_path=public");
        assert!(
            pg_probe_environment(BTreeMap::from([(
                "DATABASE_URL".into(),
                "postgresql://localhost/db?unrecognized=hidden".into()
            )]))
            .is_err()
        );
    }

    #[cfg(unix)]
    fn probe_fixture(directory: &Path, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let program = directory.join("psql-fixture");
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        program.to_str().unwrap().to_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pg_probe_waits_for_database_creation_before_allowing_migration() {
        let directory = tempfile::tempdir().unwrap();
        let program = probe_fixture(
            directory.path(),
            r#"#!/bin/sh
[ "$PGDATABASE" = "delayeddb" ] || exit 99
[ "$PGUSER" = "manageduser" ] || exit 99
[ "$7" = "SELECT 1" ] || exit 99
if [ ! -f "$PROBE_STATE/attempted" ]; then
  touch "$PROBE_STATE/attempted"
  exit 2
fi
touch "$PROBE_STATE/database-login-confirmed"
"#,
        );
        let target = BTreeMap::from([
            ("PGDATABASE".into(), "delayeddb".into()),
            ("PGUSER".into(), "manageduser".into()),
            (
                "PROBE_STATE".into(),
                directory.path().to_str().unwrap().into(),
            ),
        ]);
        wait_for_pg_targets(
            &program,
            &[target],
            Duration::from_secs(2),
            Duration::from_millis(10),
            None,
        )
        .await
        .unwrap();
        assert!(directory.path().join("attempted").exists());
        assert!(directory.path().join("database-login-confirmed").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pg_probe_deadline_kills_and_reaps_hung_client() {
        let directory = tempfile::tempdir().unwrap();
        // 启动握手（对齐孙进程端口夹具的修法）：客户端先写 pid 再等 start 闸门。
        // 否则全量并发下 150ms 预算可能在 shell 被调度前耗尽，pid 文件从未
        // 出现，断言无法成立（2026-09-27 无界并发复现，负载敏感夹具竞态）。
        let program = probe_fixture(
            directory.path(),
            r#"#!/bin/sh
printf '%s' "$$" > "$PROBE_STATE/pid"
i=0
while [ ! -f "$PROBE_STATE/start" ] && [ "$i" -lt 200 ]; do
  sleep 0.05
  i=$((i + 1))
done
exec sleep 30
"#,
        );
        let target = BTreeMap::from([(
            "PROBE_STATE".into(),
            directory.path().to_str().unwrap().into(),
        )]);
        let targets = vec![target];
        let probe = tokio::spawn(async move {
            wait_for_pg_targets(
                &program,
                &targets,
                Duration::from_millis(150),
                Duration::from_millis(10),
                None,
            )
            .await
        });
        // pid 文件出现后客户端必然在闸门处挂起，再放行进入被测的挂起段。
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(directory.path().join("pid")) {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("probe client never started");
        std::fs::write(directory.path().join("start"), b"1").unwrap();
        let result = probe.await.unwrap();
        assert!(result.is_err());
        let status = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "probe client survived deadline");
    }
}

/// Executed only by tools/test_pg_readiness_real.py against its isolated PG16.
#[cfg(test)]
mod real_pg_readiness_fixture {
    use super::*;

    #[tokio::test]
    #[ignore = "requires tools/test_pg_readiness_real.py isolated PostgreSQL fixture"]
    async fn actual_pg_delayed_database_uri() {
        let program = std::env::var("PG_READINESS_FIXTURE_PROGRAM")
            .expect("use tools/test_pg_readiness_real.py");
        let target = pg_probe_environment(std::collections::BTreeMap::from([(
            "DATABASE_URL".into(),
            "postgresql://fixture@127.0.0.1:5549/delayeddb".into(),
        )]))
        .unwrap();
        let started = std::time::Instant::now();
        wait_for_pg_targets(
            &program,
            &[target],
            Duration::from_secs(15),
            Duration::from_millis(200),
            None,
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_secs(4),
            "migration released before delayed database existed"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn migration_timeout_is_advisory_after_confirmed_tree_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("source");
    let directory = workspace.join("frontend");
    std::fs::create_dir_all(&directory).unwrap();
    let mut spec = cases::spec_with(None);
    spec.run.migrate = vec!["sh".into(), "-c".into(), "printf 'timeout-stdout\\n'; printf 'timeout-stderr\\n' >&2; echo $$ > migration.pid; exec sleep 30".into()];
    let release = workspace_manifest::ReleaseLock {
        schema_version: 1,
        release_id: "migration-timeout-fixture".into(),
        workspace_name: "migration-timeout-fixture".into(),
        pingap: workspace_manifest::LockedPingap {
            mode: workspace_manifest::PingapMode::Managed,
            config: None,
            version: "0.14.3".into(),
            commit: "fixture".into(),
        },
        minimum_app_cli_version: "0.1.3".into(),
        runtime_image_digest: "native-test-fixture".into(),
        services: vec![spec.clone()],
        bridge_service: None,
    };
    let result = run_migration_with_receipt_and_timeout(
        &spec,
        &release,
        &workspace,
        &root.path().join("logs"),
        None,
        None,
        Duration::from_millis(100),
    )
    .await;
    assert!(
        result.is_ok(),
        "a timed-out application migration must return an advisory after its tree is stopped: {result:?}"
    );
    let report = result.unwrap();
    assert!(matches!(
        report.outcome,
        MigrationOutcome::Advisory(MigrationFailure {
            kind: MigrationFailureKind::Timeout,
            ..
        })
    ));
    assert!(
        report.stdout.contains("timeout-stdout"),
        "{}",
        report.stdout
    );
    assert!(
        report.stderr.contains("timeout-stderr"),
        "{}",
        report.stderr
    );
    let out = std::fs::read_to_string(root.path().join("logs/frontend/runtime.out.log")).unwrap();
    let err = std::fs::read_to_string(root.path().join("logs/frontend/runtime.err.log")).unwrap();
    assert!(out.contains("timeout-stdout"), "{out}");
    assert!(
        err.contains("timeout-stderr") && err.contains("ERROR") && err.contains("timed out"),
        "{err}"
    );
    let pid: u32 = std::fs::read_to_string(directory.join("migration.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        !process_utils::process_group_exists(pid).unwrap(),
        "the original migration process group must be confirmed stopped"
    );
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    spec.port = reservation.local_addr().unwrap().port();
    drop(reservation);
    std::fs::write(
        directory.join("ready.txt"),
        "service-after-migration-timeout",
    )
    .unwrap();
    let argv = vec![
        "python3".into(),
        "-m".into(),
        "http.server".into(),
        spec.port.to_string(),
        "--bind".into(),
        "127.0.0.1".into(),
    ];
    let mut child = start_service(
        &spec,
        &argv,
        &workspace,
        &root.path().join("logs"),
        &release.release_id,
        None,
    )
    .await
    .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(response) = client
                .get(format!("http://127.0.0.1:{}/ready.txt", spec.port))
                .send()
                .await
                && let Ok(body) = response.text().await
                && body == "service-after-migration-timeout"
            {
                break body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert_ne!(child.stop(Duration::ZERO).await, StopOutcome::Unconfirmed);
    assert_eq!(response.unwrap(), "service-after-migration-timeout");
}

#[cfg(unix)]
fn migration_test_release(spec: &ServiceSpec) -> workspace_manifest::ReleaseLock {
    workspace_manifest::ReleaseLock {
        schema_version: 1,
        release_id: "migration-fixture".into(),
        workspace_name: "migration-fixture".into(),
        pingap: workspace_manifest::LockedPingap {
            mode: workspace_manifest::PingapMode::Managed,
            config: None,
            version: "0.14.3".into(),
            commit: "fixture".into(),
        },
        minimum_app_cli_version: "0.1.3".into(),
        runtime_image_digest: "native-test-fixture".into(),
        services: vec![spec.clone()],
        bridge_service: None,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn migration_failure_streams_full_output_and_redacts_runtime_secrets() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("source");
    std::fs::create_dir_all(workspace.join("frontend")).unwrap();
    let mut spec = cases::spec_with(None);
    spec.run.migrate = vec!["sh".into(), "-c".into(), "printf 'live-migration-output %s\\n' \"$POSTGRES_PASSWORD\"; printf '%12000s stdout-tail\\n' x; printf 'stderr-output %s\\n' \"$DATABASE_URL\" >&2; while [ ! -f release-now ]; do sleep 0.02; done; exit 1".into()];
    spec.env.insert(
        "DATABASE_URL".into(),
        "postgresql://stale:stale@localhost/db".into(),
    );
    let release = migration_test_release(&spec);
    let password = "test-migration-secret@:/%\nsecond-secret-line";
    let pg = shared_types::StartPgCredential {
        username: "fixture-user".into(),
        password: password.into(),
    };
    let logs = root.path().join("logs");
    let execution = {
        let workspace = workspace.clone();
        let logs = logs.clone();
        tokio::spawn(async move {
            run_migration_with_receipt_and_timeout(
                &spec,
                &release,
                &workspace,
                &logs,
                Some(&pg),
                None,
                Duration::from_secs(5),
            )
            .await
        })
    };
    let streamed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(content) =
                tokio::fs::read_to_string(logs.join("frontend/runtime.out.log")).await
                && content.contains("stdout-tail")
            {
                break content;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    tokio::fs::write(workspace.join("frontend/release-now"), "release")
        .await
        .unwrap();
    let report = execution.await.unwrap().unwrap();
    let live = streamed.expect("migration stdout must be visible before the script exits");
    assert!(live.contains("live-migration-output") && live.contains("[REDACTED]"));
    assert!(!live.contains(password));
    assert!(!live.contains("test-migration-secret") && !live.contains("second-secret-line"));
    assert!(matches!(
        report.outcome,
        MigrationOutcome::Advisory(MigrationFailure {
            kind: MigrationFailureKind::Exit,
            exit_code: Some(1),
            ..
        })
    ));
    assert!(report.stdout.len() > 12000 && report.stdout.contains("stdout-tail"));
    assert!(report.stderr.contains("stderr-output") && !report.stderr.contains(password));
    let err = std::fs::read_to_string(logs.join("frontend/runtime.err.log")).unwrap();
    assert!(err.contains("stderr-output") && err.contains("ERROR") && !err.contains(password));
    assert!(!err.contains("test-migration-secret") && !err.contains("second-secret-line"));
}

#[cfg(unix)]
#[tokio::test]
async fn migration_cancel_retains_partial_output_and_confirms_tree_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("source");
    let directory = workspace.join("frontend");
    std::fs::create_dir_all(&directory).unwrap();
    let mut spec = cases::spec_with(None);
    spec.run.migrate = vec!["sh".into(), "-c".into(), "printf 'partial-stdout'; printf 'partial-stderr' >&2; echo $$ > migration.pid; exec sleep 30".into()];
    let release = migration_test_release(&spec);
    let cancel = tokio_util::sync::CancellationToken::new();
    let execution = {
        let cancel = cancel.clone();
        let workspace = workspace.clone();
        let logs = root.path().join("logs");
        tokio::spawn(async move {
            run_migration_with_receipt_and_timeout(
                &spec,
                &release,
                &workspace,
                &logs,
                None,
                Some(&cancel),
                Duration::from_secs(5),
            )
            .await
        })
    };
    let pid = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(directory.join("migration.pid")).await
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    cancel.cancel();
    let report = execution.await.unwrap().unwrap();
    assert!(matches!(
        report.outcome,
        MigrationOutcome::Advisory(MigrationFailure {
            kind: MigrationFailureKind::Cancelled,
            ..
        })
    ));
    assert_eq!(report.stdout, "partial-stdout");
    assert_eq!(report.stderr, "partial-stderr");
    assert!(!process_utils::process_group_exists(pid.unwrap()).unwrap());
    assert!(
        std::fs::read_to_string(root.path().join("logs/frontend/runtime.out.log"))
            .unwrap()
            .contains("partial-stdout")
    );
    assert!(
        std::fs::read_to_string(root.path().join("logs/frontend/runtime.err.log"))
            .unwrap()
            .contains("partial-stderr")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn migration_spawn_failure_is_advisory_and_persisted() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("source");
    std::fs::create_dir_all(workspace.join("frontend")).unwrap();
    let mut spec = cases::spec_with(None);
    spec.run.migrate = vec!["/definitely-missing/migration-executable".into()];
    let release = migration_test_release(&spec);
    let report = run_migration_with_receipt_and_timeout(
        &spec,
        &release,
        &workspace,
        &root.path().join("logs"),
        None,
        None,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert!(matches!(
        report.outcome,
        MigrationOutcome::Advisory(MigrationFailure {
            kind: MigrationFailureKind::Spawn,
            ..
        })
    ));
    let err = std::fs::read_to_string(root.path().join("logs/frontend/runtime.err.log")).unwrap();
    assert!(
        err.contains("ERROR") && err.contains("spawn migration"),
        "{err}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn migration_lease_contention_skips_script_without_startup_error() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("source");
    std::fs::create_dir_all(workspace.join("frontend")).unwrap();
    let mut spec = cases::spec_with(None);
    spec.run.migrate = vec!["sh".into(), "-c".into(), "touch migration-executed".into()];
    let release = migration_test_release(&spec);
    let lease = crate::migration_journal::MigrationJournal::begin(
        &workspace,
        crate::migration_journal::identity(&release, &spec.service_id).unwrap(),
    )
    .unwrap()
    .unwrap();
    let report = run_migration_with_receipt_and_timeout(
        &spec,
        &release,
        &workspace,
        &root.path().join("logs"),
        None,
        None,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert!(matches!(
        report.outcome,
        MigrationOutcome::Advisory(MigrationFailure {
            kind: MigrationFailureKind::ExecutionLease,
            ..
        })
    ));
    assert!(
        !workspace.join("frontend/migration-executed").exists(),
        "a second migration must never execute without the lease"
    );
    drop(lease);
}

#[cfg(unix)]
#[tokio::test]
async fn migration_log_io_failure_preserves_full_capture_and_reports_the_cause() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("source");
    std::fs::create_dir_all(workspace.join("frontend")).unwrap();
    let mut spec = cases::spec_with(None);
    spec.run.migrate = vec!["sh".into(), "-c".into(), "i=0; while [ \"$i\" -lt 2048 ]; do printf 'line-%s\\n' \"$i\"; i=$((i + 1)); done; printf 'last-stderr\\n' >&2; exit 1".into()];
    let release = migration_test_release(&spec);
    let log_dir = root.path().join("file-blocking-log-directory");
    std::fs::write(&log_dir, "regular file").unwrap();
    let report = run_migration_with_receipt_and_timeout(
        &spec,
        &release,
        &workspace,
        &log_dir,
        None,
        None,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(report.stdout.lines().count(), 2048);
    assert!(report.stdout.starts_with("line-0\n") && report.stdout.ends_with("line-2047\n"));
    assert_eq!(report.stderr, "last-stderr\n");
    let MigrationOutcome::Advisory(failure) = report.outcome else {
        panic!("log failure must be diagnostic");
    };
    assert_eq!(failure.kind, MigrationFailureKind::Exit);
    assert!(
        failure.detail.contains("open migration")
            && failure.detail.contains("persist migration result"),
        "{}",
        failure.detail
    );
}
