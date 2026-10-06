use std::sync::Arc;

use anyhow::Context;
use clap::Parser;

use app_cli::CliArgs;

fn main() -> anyhow::Result<()> {
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--app-cli-container-stop"))
    {
        anyhow::ensure!(
            std::env::args_os().count() == 3,
            "container stop requires workspace"
        );
        let root = std::env::args_os()
            .nth(2)
            .context("container stop workspace missing")?;
        return runtime_supervisor::runtime()?.block_on(
            app_cli::supervision::drain_for_container_stop(std::path::Path::new(&root)),
        );
    }
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--app-cli-domain-recovery"))
    {
        return app_cli::supervision::physical_domain_recovery(std::env::args_os().skip(2));
    }
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--app-cli-cleanup-engine"))
    {
        anyhow::ensure!(
            std::env::args_os().count() == 2,
            "unexpected cleanup argument"
        );
        return runtime_supervisor::runtime()?
            .block_on(app_cli::supervision::cleanup_external_engine());
    }
    if let Some(result) = runtime_supervisor::auxiliary_entry() {
        std::process::exit(result?);
    }
    let command = CliArgs::parse().command;
    if let app_cli::config::Command::Validate(args) = command {
        let result = match runtime_supervisor::runtime() {
            Ok(runtime) => {
                let result = runtime.block_on(app_cli::validate::run(
                    &args.workspace.workspace,
                    args.dev,
                    args.json,
                ));
                // A timed-out DNS resolver may still be running on a blocking thread.
                // Validation has finished; its runtime must not extend the deadline.
                runtime.shutdown_background();
                result
            }
            Err(_) => app_cli::validate::write_initialization_failure(
                &args.workspace.workspace,
                args.dev,
                args.json,
            )
            .map(|()| 3),
        };
        let exit_code = match result {
            Ok(code) => code,
            Err(error) => {
                eprintln!("configuration validation failed: {error}");
                3
            }
        };
        // The async report writer has completed and flushed before process exit.
        std::process::exit(exit_code);
    }
    runtime_supervisor::runtime()?.block_on(run(command))
}

async fn run(command: app_cli::config::Command) -> anyhow::Result<()> {
    let args = match command {
        app_cli::config::Command::Validate(_) => {
            anyhow::bail!("validate must use the read-only entry point");
        }
        app_cli::config::Command::GenLock(args) => {
            return app_cli::devtool::gen_lock(&args.workspace.workspace, args.dev).await;
        }
        app_cli::config::Command::Build(args) => {
            app_cli::build::run(
                &args.workspace.workspace,
                args.dev,
                args.deploy_dir.as_deref(),
                args.only.as_deref(),
            )?;
            return Ok(());
        }
        app_cli::config::Command::Serve(args) => {
            let runtime = app_cli::RuntimeArgs::from(args);
            // tracing 先于监督（recovery v2 plan §9）：owner 進入恢复围栏时仍
            // 要有 stderr/文件日志——围栏判定发生在监督内部，事后初始化的
            // 日志链什么都看不到（app 11 零日志排障代价的教训）。
            let _guard = init_tracing(&runtime.log_dir);
            return app_cli::server::serve(&runtime).await;
        }
        app_cli::config::Command::Owner(args) => return app_cli::supervision::control(&args).await,
        app_cli::config::Command::RunService(args) => {
            let _guard = init_tracing(&args.log_dir);
            return app_cli::run_service::run(&args.release_id, &args.service_id, &args.log_dir);
        }
        // 纯只读查询：在运行初始化/日志目录/owner 获取之前分派——无人监听时
        // 不启动 owner（Plan §5.3）；结果 JSON 写 stdout、诊断写 stderr。
        app_cli::config::Command::Readiness(args) => {
            return match app_cli::readiness_query::run(&args.admin_addr).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    eprintln!("readiness query failed: [{}] {}", error.code, error.message);
                    let structured = serde_json::json!({
                        "error": true,
                        "code": error.code,
                        "message": error.message,
                    });
                    println!("{structured}");
                    std::process::exit(error.exit_code);
                }
            };
        }
        app_cli::config::Command::Journal {
            command: app_cli::config::JournalCommand::Adopt(args),
        } => {
            let report = app_cli::server::journal::adopt_superseded_legacy(
                &args.workspace.workspace,
                args.force,
            )?;
            println!("{report}");
            return Ok(());
        }
        app_cli::config::Command::Run(args) => app_cli::RuntimeArgs::from(args),
    };
    let args = args.for_management()?;
    // tracing 先于监督（同 Serve 分支注释）：run 的 owner 围栏/清理阶段
    // 同样需要早期日志。
    let _guard = init_tracing(&args.log_dir);
    // An ordinary run is an explicit client request. The permanent serve
    // owner has its own process/lifetime; CLI failure or business Stop cannot
    // terminate management. Only an authenticated legacy worker retains the
    // old same-owner business body during an existing upgrade window.
    let state_root = app_cli::supervision::scope(&args.workspace)?;
    if runtime_supervisor::Worker::from_env(&state_root)
        .await?
        .is_none()
    {
        return app_cli::control::run_client::run(&args).await;
    }
    let runtime_status = app_cli::runtime_status::RuntimeStatusService::default();

    // idle 判定（仅无部署请求且无 release.lock 时进入）——无副作用常驻探针。
    // deploy_requested() 检查不涉及3010，可在 bind 前安全调用。
    if !app_cli::deploy::deploy_requested()
        && !tokio::fs::try_exists(args.workspace.join("release.lock.toml"))
            .await
            .unwrap_or(false)
    {
        // The supervised empty workspace still needs a cancellable management
        // path. Reuse serve's idle API instead of an unobservable idle loop.
        return app_cli::server::serve(&args).await;
    }

    // 管理 API 预绑定（P1-01）：bind 成功才进入 deploy_stage/supervisor——
    // 端口冲突在一切业务副作用之前 fail-fast（非零退出，不下载制品、不 spawn
    // 任何用户服务）。legacy 形态以静态 ServerState 承载（初始化期 /v1/deploy
    // 等写端点由 initializing 门控拒绝，/health 恒200覆盖 kubelet liveness）。
    let legacy_state =
        std::sync::Arc::new(app_cli::server::ServerState::new(runtime_status.clone()));
    if let Ok(release) = app_cli::manifest::read_release_lock(&args.workspace) {
        legacy_state.set_release(release);
        legacy_state.set_phase(app_cli::server::ServerPhase::Running);
    }
    let api_addr = args.admin_addr.clone();
    let api_log_dir = args.log_dir.clone();
    let api_workspace = args.workspace.clone();
    let api_pingap_bin = args.pingap_bin.clone();
    let api_state = legacy_state.clone();
    let (api_listener, api_app) = app_cli::api::bind(
        &api_addr,
        api_workspace,
        api_log_dir,
        api_pingap_bin,
        api_state,
    )
    .await?;
    let api_failure = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let api_monitor_failure = api_failure.clone();
    let supervisor_cancel = tokio_util::sync::CancellationToken::new();
    let api_cancel = supervisor_cancel.clone();
    let api_handle = tokio::spawn(async move {
        let result = axum::serve(api_listener, api_app)
            .await
            .context("serve app-cli management API");
        if let Err(error) = result {
            tracing::error!("app-cli management API failed: {error:#}");
            if let Ok(mut slot) = api_monitor_failure.lock() {
                *slot = Some(format!("{error:#}"));
            }
        }
        api_cancel.cancel();
    });
    let worker =
        runtime_supervisor::Worker::from_env(&app_cli::supervision::scope(&args.workspace)?)
            .await?;
    let _worker_control = match worker {
        Some(worker) => Some(
            worker
                .serve(Arc::new(app_cli::supervision::ForegroundControl {
                    cancellation: supervisor_cancel.clone(),
                }))
                .await?,
        ),
        None => None,
    };

    // 部署段（仅 env 有 APP_DEPLOY_URL 时执行）：API 已绑定3010，kubelet
    // liveness 由 /health 覆盖；deploy 期间 /ready 503（initializing 门控）。
    // bind 失败已在上文 fail-fast，此处无端口冲突风险。
    deploy_stage(&args).await?;
    // deploy 成功后开放写端点（supervisor 接管业务前初始化完成）
    legacy_state.mark_initialized();

    // supervisor（前台阻塞，退出 → main 退出 → supervisor [program:app] 重启）。
    // P1-02：保留原始错误——supervisor Err 决定进程退出码。
    // R08：直跑形态无操作上下文——dev 信号 env 兜底、无每操作凭据。
    let supervisor_result = app_cli::supervisor::run_with_cancel(
        args.clone(),
        runtime_status,
        supervisor_cancel,
        None,
        app_cli::supervisor::legacy_run_profile(),
    )
    .await;
    match &supervisor_result {
        Ok(()) => tracing::info!("app-cli supervisor exited normally"),
        Err(e) => tracing::error!("app-cli supervisor error: {e:#}"),
    }

    api_handle.abort();
    if let Ok(slot) = api_failure.lock()
        && let Some(api_error) = slot.as_ref()
    {
        anyhow::bail!("app-cli management API terminated: {api_error}");
    }
    supervisor_result?;
    Ok(())
}

/// 初始化 tracing：stderr（彩色）+ 文件（daily 轮转 + non-blocking）。
/// 返回 WorkerGuard（调用方保活到程序退出，保证日志刷盘）。
fn init_tracing(log_dir: &std::path::Path) -> Arc<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "app_cli=info".into());

    // 文件 Layer：daily 轮转，写到 <log_dir>/app-cli.log.<date>
    let file = tracing_appender::rolling::daily(log_dir, "app-cli.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    let guard = Arc::new(guard);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        // 文件层 JSON 行格式（flatten_event 平铺 message，去掉 span 结构）：
        // 输出 {"timestamp","level","message"} 顶层三键，与 /v1/logs 的 orchestrator
        // 内置源解析（log::read parse_line Jsonl 分支）直接匹配——编排日志因此
        // 支持 levels/since/until 过滤。人类阅读走 stderr / supervisord out 文本。
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(false)
                .with_span_list(false)
                .with_writer(non_blocking),
        )
        .init();

    guard
}

/// 部署段（**仅 legacy 直跑形态**）：env 有 `APP_DEPLOY_URL` 才执行。
/// API 已在本函数调用前绑定3010（P1-01），kubelet liveness 由 /health
/// 覆盖，/ready 在初始化期间503（initializing 门控）。不再需要 LivenessHold。
async fn deploy_stage(args: &app_cli::RuntimeArgs) -> anyhow::Result<()> {
    if !app_cli::deploy::deploy_requested() {
        return Ok(());
    }
    let deploy_result = app_cli::deploy::run_from_env(&args.workspace).await;
    if let Err(e) = deploy_result {
        tracing::error!("❌ deploy stage failed: {e:#}");
        anyhow::bail!("deploy stage failed");
    }
    Ok(())
}
