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

    // 本地编译工具：与 --gen-lock 同类的本地分派。必须先于 init_tracing——
    // 宿主机裸跑没有 /app/logs（默认 log_dir），tracing-appender 建目录会失败；
    // build 自身只 println 输出，不依赖 tracing/日志目录。
    if let Some(app_cli::config::Command::Build {
        dev,
        deploy_dir,
        only,
    }) = &args.command
    {
        app_cli::build::run(&args.workspace, *dev, deploy_dir.as_deref(), only.as_deref())?;
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
    //
    // **仅 serve / legacy 直跑形态执行**：run-service 是已编排服务的 exec 载体，
    // 而容器 env 恒带 APP_DEPLOY_URL 三元组（serve 的部署种子，换 Pod 模式每次
    // 更新）——run-service 若也执行部署段会二次部署（move code 到 .previous 跨
    // 卷 link 失败 exit 1 → supervisord SPAWN_ERROR，全部 app-svc-* 拉不起来）。
    // build 是本地编译工具，无运行时副作用，不进部署段（已在前置本地分派返回）。
    match &args.command {
        // 结构性不可达（Build 在 init_tracing 前已分派）；与 run-service 的
        // unreachable 同款：守住 match 穷尽性，防止后续重构把分派挪走后静默走错路径。
        Some(app_cli::config::Command::Build { .. }) => {
            unreachable!("build dispatched before tracing init")
        }
        Some(app_cli::config::Command::Serve) => {
            deploy_stage(&args).await?;
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
        None => {
            deploy_stage(&args).await?;
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

    // 管理 API（后台并发跑；supervisor 退出时 abort）——legacy 形态以静态
    // ServerState 承载（读 lock 后直接 Running 相位，readiness 跟随 runtime_status）
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
    let api_handle = tokio::spawn(async move {
        if let Err(error) = app_cli::api::serve(
            &api_addr,
            api_workspace,
            api_log_dir,
            api_pingap_bin,
            api_state,
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

/// 部署段（serve / legacy 直跑形态公用）：env 有 `APP_DEPLOY_URL` 才执行；
/// liveness 托管占位 3010 防下载窗口 kubelet 误杀，失败上抛（supervisord 重试）。
async fn deploy_stage(args: &app_cli::CliArgs) -> anyhow::Result<()> {
    if !app_cli::deploy::deploy_requested() {
        return Ok(());
    }
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
    Ok(())
}
