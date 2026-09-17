use std::sync::Arc;

use anyhow::Context;
use clap::Parser;

use app_cli::CliArgs;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();

    // 本地开发子模式：--gen-lock <workspace> 只生成 release.lock + 预览 Pingap 配置后退出。
    if let Some(workspace) = args.gen_lock.clone() {
        app_cli::devtool::gen_lock(&workspace).await?;
        return Ok(());
    }

    // 本地编译工具：与 --gen-lock 同类的本地分派。必须先于 init_tracing——
    // 宿主机裸跑没有 /app/logs（默认 log_dir），tracing-appender 建目录会失败；
    // build 自身只 println 输出，不依赖 tracing/日志目录。
    if let Some(app_cli::config::Command::Build {
        dev,
        deploy_dir,
        only,
    }) = &args.command
    {
        app_cli::build::run(
            &args.workspace,
            *dev,
            deploy_dir.as_deref(),
            only.as_deref(),
        )?;
        return Ok(());
    }

    // legacy 直跑形态（无子命令）：P1-01 修复——API 先于 deploy_stage 绑定3010。
    // init tracing 必须在子命令分派前（serve/attach 需要日志输出）。
    // Build 已在 init_tracing 前分派返回（日志目录可能不存在）。
    let _guard = init_tracing(&args.log_dir);

    match &args.command {
        Some(app_cli::config::Command::Build { .. }) => {
            unreachable!("build dispatched before tracing init")
        }
        Some(app_cli::config::Command::Serve) => {
            return app_cli::server::serve(&args).await;
        }
        Some(app_cli::config::Command::RunService {
            release_id,
            service_id,
        }) => {
            if let Err(e) = app_cli::run_service::run(release_id, service_id, &args) {
                eprintln!("run-service {release_id}/{service_id}: {e:#}");
                std::process::exit(1);
            }
            unreachable!("exec replaced process image");
        }
        None => {} // legacy 直跑路径：下方继续
    }

    // ── legacy 直跑路径 ──
    let runtime_status = app_cli::runtime_status::RuntimeStatusService::default();

    tracing::info!(
        "app-cli starting: workspace={} log_dir={} admin={} pingap_bin={}",
        args.workspace.display(),
        args.log_dir.display(),
        args.admin_addr,
        args.pingap_bin.display()
    );

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
async fn deploy_stage(args: &app_cli::CliArgs) -> anyhow::Result<()> {
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
