use std::sync::Arc;

use clap::Parser;

use app_cli::CliArgs;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();

    // init tracing：stderr + 文件（daily 轮转 + non-blocking，_guard 保活到 main 退出）
    let _guard = init_tracing(&args.log_dir);

    tracing::info!(
        "app-cli starting: workspace={} log_dir={} admin={} pingap_bin={}",
        args.workspace.display(),
        args.log_dir.display(),
        args.admin_addr,
        args.pingap_bin.display()
    );

    // 管理 API（后台并发跑；supervisor 退出时 abort）
    let api_addr = args.admin_addr.clone();
    let api_log_dir = args.log_dir.clone();
    let api_handle = tokio::spawn(async move {
        app_cli::api::serve(&api_addr, api_log_dir).await;
    });

    // supervisor（前台阻塞，退出 → main 退出 → supervisor [program:app] 重启）
    match app_cli::supervisor::run(&args).await {
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
