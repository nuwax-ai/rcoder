use std::sync::Arc;

use anyhow::Context;
use clap::Parser;

use app_cli::CliArgs;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = match CliArgs::parse().command {
        app_cli::config::Command::GenLock(args) => {
            return app_cli::devtool::gen_lock(&args.workspace).await;
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
            let _guard = init_tracing(&runtime.log_dir);
            return app_cli::server::serve(&runtime).await;
        }
        app_cli::config::Command::RunService(args) => {
            let _guard = init_tracing(&args.log_dir);
            return app_cli::run_service::run(&args.release_id, &args.service_id, &args.log_dir);
        }
        app_cli::config::Command::Run(args) => app_cli::RuntimeArgs::from(args),
    };
    let _guard = init_tracing(&args.log_dir);

    // ── legacy 直跑路径 ──
    let runtime_status = app_cli::runtime_status::RuntimeStatusService::default();

    tracing::info!(
        "app-cli starting: workspace={} log_dir={} admin={} pingap_bin={}",
        args.workspace.display(),
        args.log_dir.display(),
        args.admin_addr,
        args.pingap_bin.display()
    );

    // R02：旧命令也进入身份/锁/客户端分派——OwnerGuard 先于一切运行态副作用。
    // 已有 serve owner（锁被活进程持有）→ 转交唯一 owner（运行 API 提交
    // Start 并等终态）；身份不符/协议不兼容/凭据缺失 → 明确拒绝；无人持锁
    // → 本地 legacy 编排（持锁运行，后续 CLI 同样被分派或拒绝）。
    {
        let application_id = std::env::var("PROJECT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown-app".to_string());
        let state_root =
            app_cli::runtime_kernel::RuntimeStore::resolve_root(&args.workspace, &application_id)?;
        match app_cli::platform::owner_guard::OwnerGuard::acquire(&state_root) {
            Ok(guard) => {
                // 持锁运行：guard 存活到进程退出（后续 CLI 的 acquire 失败进分派）
                std::mem::forget(guard);
            }
            Err(_) => {
                tracing::info!(
                    "owner lock held; dispatching to the running owner at {}",
                    args.admin_addr
                );
                let dispatch = app_cli::owner_dispatch::dispatch_to_owner(
                    &args.admin_addr,
                    &args.workspace,
                    &state_root,
                    &application_id,
                )
                .await?;
                match dispatch {
                    app_cli::owner_dispatch::OwnerDispatch::Terminal(view) => {
                        return app_cli::owner_dispatch::describe_terminal(&view);
                    }
                    app_cli::owner_dispatch::OwnerDispatch::NoOwner => {
                        // 锁被持有但探测无果——dispatch_to_owner 已在 None 路径
                        // 明确报错；此分支不可达，防御性走本地（保持旧行为）
                        tracing::warn!("owner dispatch returned NoOwner unexpectedly");
                    }
                }
            }
        }
    }

    // idle 判定（仅无部署请求且无 release.lock 时进入）——无副作用常驻探针。
    // deploy_requested() 检查不涉及3010，可在 bind 前安全调用。
    if !app_cli::deploy::deploy_requested()
        && !tokio::fs::try_exists(args.workspace.join("release.lock.toml"))
            .await
            .unwrap_or(false)
    {
        app_cli::idle::serve_forever(&args.admin_addr).await;
        return Ok(());
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

    // 部署段（仅 env 有 APP_DEPLOY_URL 时执行）：API 已绑定3010，kubelet
    // liveness 由 /health 覆盖；deploy 期间 /ready 503（initializing 门控）。
    // bind 失败已在上文 fail-fast，此处无端口冲突风险。
    deploy_stage(&args).await?;
    // deploy 成功后开放写端点（supervisor 接管业务前初始化完成）
    legacy_state.mark_initialized();

    // supervisor（前台阻塞，退出 → main 退出 → supervisor [program:app] 重启）。
    // P1-02：保留原始错误——supervisor Err 决定进程退出码。
    let supervisor_result = app_cli::supervisor::run_with_cancel(
        args.clone(),
        runtime_status,
        supervisor_cancel,
        None,
        true,
        // R08：直跑形态无操作上下文——env 兜底（server 形态经操作显式传递）
        app_cli::supervisor::dev_run_profile(),
        // 直跑形态无每操作凭据（进程 env 透传）
        None,
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
