use std::sync::Arc;

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

    let runtime_status = app_cli::runtime_status::RuntimeStatusService::default();

    // init tracing：stderr + 文件（daily 轮转 + non-blocking，_guard 保活到 main 退出）
    let _guard = init_tracing(&args.log_dir);

    tracing::info!(
        "app-cli starting: workspace={} log_dir={} admin={} pingap_bin={}",
        args.workspace.display(),
        args.log_dir.display(),
        args.admin_addr,
        args.pingap_bin.display()
    );

    // 部署段（生产 RBD 卷形态）：APP_DEPLOY_URL 注入时下载制品包并切换 code/。
    // 必须先于 api / 编排读 release.lock（api::serve 与 supervisor 都读 lock）；
    // 首次部署无 code 时 api 起不来，:3010 由部署段的 liveness 托管应答探针
    // （防大制品下载窗口 kubelet 误杀）。失败退出非零 → supervisord 重试
    // （code/ 现场不破坏，readiness 超时由 rcoder wait_app_ready 上报）。
    if app_cli::deploy::deploy_requested() {
        // 占位失败（端口被占等）不阻断部署本身：warn 后裸跑（最坏 liveness 抖动）
        let hold = match app_cli::deploy::LivenessHold::start(&args.admin_addr) {
            Ok(hold) => Some(hold),
            Err(e) => {
                tracing::warn!(
                    "liveness hold bind {} failed, deploy continues without hold: {e:#}",
                    args.admin_addr
                );
                None
            }
        };
        let deploy_result = app_cli::deploy::run_from_env(&args.workspace).await;
        if let Some(hold) = hold {
            hold.release().await;
        }
        if let Err(e) = deploy_result {
            tracing::error!("❌ deploy stage failed: {e:#}");
            anyhow::bail!("deploy stage failed");
        }
    }

    // idle 判定：未部署（无 release.lock——start 无 url 创建的空容器）→ 最小形态
    // 常驻应答探针（防 kubelet liveness 杀容器），等 start{url} 部署换 Pod 替换本
    // 进程。lock 存在但损坏不进 idle——走下方正常链 fail-fast（supervisord 重试
    // 后 FATAL，损坏 lock 是需人工介入的异常态，静默 idle 会掩盖问题）。
    if !args.workspace.join("release.lock.toml").exists() {
        app_cli::idle::serve_forever(&args.admin_addr).await;
        return Ok(()); // 仅 SIGTERM（容器终止/被替换）到达
    }

    // 管理 API（后台并发跑；supervisor 退出时 abort）
    let api_addr = args.admin_addr.clone();
    let api_log_dir = args.log_dir.clone();
    let api_workspace = args.workspace.clone();
    let api_pingap_bin = args.pingap_bin.clone();
    let api_runtime_status = runtime_status.clone();
    let api_handle = tokio::spawn(async move {
        if let Err(error) = app_cli::api::serve(
            &api_addr,
            api_workspace,
            api_log_dir,
            api_pingap_bin,
            api_runtime_status,
        )
        .await
        {
            tracing::error!("app-cli API failed: {error:#}");
        }
    });

    // supervisor（前台阻塞，退出 → main 退出 → supervisor [program:app] 重启）
    match app_cli::supervisor::run(&args, runtime_status).await {
        Ok(()) => tracing::info!("app-cli supervisor exited normally"),
        Err(e) => tracing::error!("app-cli supervisor error: {e:#}"),
    }

    api_handle.abort();
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
        .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
        .init();

    guard
}
