use clap::Parser;

use app_cli::CliArgs;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // init tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "app_cli=info".into()),
        )
        .init();

    let args = CliArgs::parse();
    tracing::info!(
        "app-cli starting: workspace={} log_dir={} admin={} pingap_bin={}",
        args.workspace.display(),
        args.log_dir.display(),
        args.admin_addr,
        args.pingap_bin.display()
    );

    // 管理 API（后台并发跑；supervisor 退出时 abort）
    let api_addr = args.admin_addr.clone();
    let api_handle = tokio::spawn(async move {
        app_cli::api::serve(&api_addr).await;
    });

    // supervisor（前台阻塞，退出 → main 退出 → supervisor [program:app] 重启）
    match app_cli::supervisor::run(&args).await {
        Ok(()) => tracing::info!("app-cli supervisor exited normally"),
        Err(e) => tracing::error!("app-cli supervisor error: {e:#}"),
    }

    api_handle.abort();
    Ok(())
}
