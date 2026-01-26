use clap::Parser;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

// 🆕 使用共享的遥测模块
use rcoder_telemetry::{TelemetryConfig, TelemetryGuard};

mod agent_runtime;
mod api_key_manager;
mod config;
mod grpc;
mod handler;
mod model;
mod process_reaper;
mod proxy_agent;

// 🔥 Pyroscope Profiler 模块（可选：需要 pyroscope feature）
#[cfg(feature = "pyroscope")]
mod profiler;

// 🔥 OpenTelemetry 追踪模块（可选：保留用于向后兼容）
#[allow(dead_code)]
mod otel_tracing;

mod router;
mod service;
mod utils;

use agent_runtime::AgentRuntime;
use config::{CliArgs, load_config_with_args};
use model::*;
use proxy_agent::cleanup_task::{CleanupConfig, start_cleanup_task};
use rcoder_proxy::{PingoraServerManager, ProxyConfig};
use router::AppState;
use std::panic;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};

/// 🔥 设置自定义 Panic Hook
///
/// 当 agent_runner panic 时，将完整的 panic 信息（包括 backtrace）写入日志文件
/// 这样即使容器被销毁，也能通过挂载的日志目录找到崩溃原因
fn set_panic_hook() {
    let default_hook = panic::take_hook();

    panic::set_hook(Box::new(move |panic_info| {
        // 🔥 立即写入日志文件（不依赖 tracing，确保在 panic 时也能写入）
        if let Err(e) = write_panic_to_file(panic_info) {
            // 如果文件写入失败，尝试输出到 stderr
            eprintln!("❌ [PANIC] 写入 panic 日志文件失败: {}", e);
        }

        // 🔥 同时输出到 stderr（Docker 会捕获到容器日志）
        eprintln!("═══════════════════════════════════════════════════════════");
        eprintln!("❌ [PANIC] agent_runner 发生致命错误！");
        eprintln!("═══════════════════════════════════════════════════════════");
        if let Some(location) = panic_info.location() {
            eprintln!("panic.location: {}:{}:{}", location.file(), location.line(), location.column());
        }
        eprintln!("panic.payload: {}", panic_info);
        eprintln!("═══════════════════════════════════════════════════════════");

        // 调用默认 hook（会终止进程）
        default_hook(panic_info);
    }));
}

/// 将 panic 信息写入日志文件
fn write_panic_to_file(panic_info: &panic::PanicHookInfo) -> std::io::Result<()> {
    // 🔥 日志文件路径：/app/container-logs/agent_runner_panic.log（使用已有的挂载目录）
    let log_path = PathBuf::from("/app/container-logs/agent_runner_panic.log");

    // 确保目录存在
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 打开文件（追加模式）
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    // 获取当前时间
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");

    // 写入 panic 信息
    writeln!(file, "═══════════════════════════════════════════════════════════")?;
    writeln!(file, "❌ [PANIC] agent_runner 发生致命错误！")?;
    writeln!(file, "时间: {}", now)?;
    writeln!(file, "═══════════════════════════════════════════════════════════")?;
    if let Some(location) = panic_info.location() {
        writeln!(file, "panic.location: {}:{}:{}", location.file(), location.line(), location.column())?;
    }
    writeln!(file, "panic.payload: {}", panic_info)?;

    // 写入 backtrace（如果启用）
    #[cfg(feature = "backtrace")]
    {
        if let Ok(backtrace) = std::backtrace::Backtrace::capture() {
            writeln!(file, "Backtrace:\n{}", backtrace)?;
        }
    }

    writeln!(file, "═══════════════════════════════════════════════════════════\n")?;

    // 强制刷新到磁盘
    file.flush()?;

    eprintln!("✅ Panic 信息已写入: {}", log_path.display());

    Ok(())
}

/// 🔥 设置优雅关闭信号处理器
///
/// 监听系统信号，实现优雅关闭：
/// - SIGTERM: Docker stop 信号
/// - SIGINT: Ctrl+C 信号
fn setup_shutdown_handler() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // 监听 SIGTERM（Docker stop）
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("❌ [SIGNAL] 无法注册 SIGTERM 处理器: {}", e);
                return;
            }
        };

        // 监听 SIGINT（Ctrl+C）
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("❌ [SIGNAL] 无法注册 SIGINT 处理器: {}", e);
                return;
            }
        };

        tokio::select! {
            _ = sigterm.recv() => {
                eprintln!("📨 [SIGNAL] 收到 SIGTERM 信号（Docker stop），开始优雅关闭...");
                write_shutdown_log("SIGTERM");
            }
            _ = sigint.recv() => {
                eprintln!("📨 [SIGNAL] 收到 SIGINT 信号（Ctrl+C），开始优雅关闭...");
                write_shutdown_log("SIGINT");
            }
        }

        eprintln!("🧹 [SIGNAL] 正在清理资源...");
        // 这里可以添加更多清理逻辑，比如：
        // - 关闭网络连接
        // - 保存状态到磁盘
        // - 通知客户端服务即将关闭

        eprintln!("✅ [SIGNAL] 优雅关闭完成，程序退出");
        std::process::exit(0);
    })
}

/// 将关闭事件写入日志文件
fn write_shutdown_log(signal: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;

    let log_path = PathBuf::from("/app/container-logs/agent_runner_shutdown.log");

    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        let _ = writeln!(file, "═══════════════════════════════════════════════════════════");
        let _ = writeln!(file, "📨 [SHUTDOWN] agent_runner 收到关闭信号");
        let _ = writeln!(file, "信号类型: {}", signal);
        let _ = writeln!(file, "时间: {}", now);
        let _ = writeln!(file, "═══════════════════════════════════════════════════════════\n");
        let _ = file.flush();
        eprintln!("✅ 关闭信息已写入: {}", log_path.display());
    }
}

// 路由创建函数已移动到 handler 模块

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 🔥 设置自定义 Panic Hook，确保 panic 信息被记录
    set_panic_hook();

    // 🔥 设置信号处理器，实现优雅关闭（Docker stop、Ctrl+C）
    let _shutdown_handle = setup_shutdown_handler();

    // ✅ 初始化 Rustls CryptoProvider（必须在最前面，在任何可能使用 TLS 的代码之前）
    // 🔥 如果这里失败，会导致 panic，但 panic hook 会捕获并记录
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("❌ [FATAL] Rustls CryptoProvider 初始化失败，程序无法继续运行。这通常是系统环境问题。");

    // 🆕 初始化遥测系统（使用 rcoder-telemetry，包含控制台 + 文件日志）
    let telemetry_config = TelemetryConfig::from_env("agent_runner").with_file_log("agent-runner"); // 启用文件日志，前缀为 agent-runner
    let telemetry: TelemetryGuard = rcoder_telemetry::init(telemetry_config).await?;
    let telemetry = Arc::new(telemetry);

    // 🆕 Pyroscope Profiler 初始化（可选：需要 pyroscope feature）
    #[cfg(feature = "pyroscope")]
    let _pyroscope_guard: Option<profiler::ProfilerGuard> = {
        info!("Pyroscope profiling feature enabled");
        match profiler::init_pyroscope_profiler_default() {
            Ok(guard) => {
                info!("Pyroscope profiler initialized successfully");
                Some(guard)
            }
            Err(e) => {
                warn!("Failed to initialize Pyroscope profiler: {}", e);
                warn!("Continuing without Pyroscope profiling");
                None
            }
        }
    };

    #[cfg(not(feature = "pyroscope"))]
    let _pyroscope_guard: Option<()> = None;

    info!("Starting rcoder - AI-powered development platform");

    // 解析命令行参数
    let cli_args = CliArgs::parse();

    // 加载配置（包含命令行参数）
    let config = load_config_with_args(cli_args);

    // 🔥 创建 AgentRuntime（新架构）
    let (agent_runtime, task_receiver) = AgentRuntime::new(1000);
    let agent_runtime = Arc::new(agent_runtime);
    info!("🔧 [MAIN] AgentRuntime 已创建");

    // 🔥 启动 Worker（在主运行时中，无需独立线程）
    agent_runtime.start(task_receiver).await;
    info!("📌 [MAIN] Agent Worker 已启动");

    // 🔥 启动健康检查和重启任务
    let health_monitor = spawn_health_monitor(agent_runtime.clone());
    info!("🔍 [MAIN] Worker 健康监控任务已启动");

    // 🔥 启动僵尸进程回收器（PID 1 必须回收孤儿进程）
    let _reaper_handle = process_reaper::start_process_reaper();
    info!("🧹 [MAIN] 僵尸进程回收器已启动 (PID 1 模式)");

    // 🆕 从配置中获取 Agent 清理配置，或使用默认值
    let agent_cleanup_config = config.agent_cleanup.clone().unwrap_or_default();
    let cleanup_config = CleanupConfig {
        idle_timeout: Duration::from_secs(agent_cleanup_config.idle_timeout_secs),
        cleanup_interval: Duration::from_secs(agent_cleanup_config.cleanup_interval_secs),
    };

    info!(
        "🧹 [MAIN] Agent 清理配置: idle_timeout={}秒, cleanup_interval={}秒",
        agent_cleanup_config.idle_timeout_secs, agent_cleanup_config.cleanup_interval_secs
    );

    // 在主异步运行时中启动清理任务
    let _cleanup_handle = start_cleanup_task(cleanup_config.clone());

    // proxy_manager 不需要直接访问 app_state，通过参数传递即可

    // 🔒 创建共享的 API 密钥 DashMap（在 Pingora 服务之前创建）
    // 这个 DashMap 将在 agent_runner 和 Pingora 之间共享
    let shared_api_key_manager =
        Arc::new(dashmap::DashMap::<String, shared_types::ModelProviderConfig>::new());
    info!("🔑 [MAIN] 共享 API 密钥 DashMap 已创建");

    // 启动代理服务（如果启用）
    let (proxy_handle, pingora_service_opt) = if let Some(proxy_config) = &config.proxy_config {
        info!(
            "启动 Pingora 反向代理服务，监听端口: {}",
            proxy_config.listen_port
        );
        info!(
            "代理路由格式: /proxy/{{port}}{{/path}} - 例如: /proxy/{}/health",
            config.port
        );

        let pingora_config = ProxyConfig {
            listen_port: proxy_config.listen_port,
            default_backend_port: proxy_config.default_backend_port,
            backend_host: proxy_config.backend_host.clone(),
            port_param: proxy_config.port_param.clone(),
            config_file: None,
            verbose: false,
        };

        // 创建 Pingora 服务器管理器，并传入共享的 API 密钥管理器
        let mut server_manager = PingoraServerManager::new(pingora_config)
            .with_api_key_manager(shared_api_key_manager.clone());

        let pingora_service = server_manager.service();
        // 启动健康检查循环（按配置）
        if proxy_config.health_check.enabled {
            let hc = &proxy_config.health_check;
            pingora_service
                .start_health_check_loop(hc.interval_seconds, hc.timeout_seconds * 1000 );
        }

        // 在后台任务中启动 Pingora 服务器
        let handle = tokio::spawn(async move {
            if let Err(e) = server_manager.start().await {
                error!("Pingora 代理服务器启动失败: {}", e);
            }
        });

        (Some(handle), Some(pingora_service))
    } else {
        (None, None)
    };

    // 🔥 创建 ApiKeyManager 包装器（包装共享 DashMap，消除双重存储）
    let api_key_manager = Arc::new(api_key_manager::ApiKeyManager::from_shared(
        shared_api_key_manager.clone(),
    ));

    // 🔒 project_id -> service_uuid 映射
    let project_uuid_map = Arc::new(DashMap::new());

    let state = Arc::new(AppState {
        sessions: Arc::new(DashMap::new()),
        config: config.clone(),
        local_task_sender: agent_runtime.clone(), // 🆕 新架构：使用 AgentRuntime
        agent_runtime: agent_runtime.clone(),
        pingora_service: pingora_service_opt,
        api_key_manager, // 现在包装 shared_api_key_manager
        shared_api_key_manager,
        project_uuid_map,
    });

    // 创建路由（传入遥测 guard 用于 /metrics 端点）
    let app = router::create_router(state.clone(), Some(telemetry.clone()));

    // 启动 gRPC 服务器
    let grpc_port = shared_types::GRPC_DEFAULT_PORT;
    let grpc_addr = format!("[::]:{}", grpc_port)
        .parse()
        .map_err(|e| anyhow::anyhow!("gRPC 地址解析失败: {}", e))?;

    // gRPC 消息大小限制：使用 shared_types 统一常量
    let grpc_service = shared_types::grpc::agent_service_server::AgentServiceServer::new(
        grpc::AgentServiceImpl::new(state.clone()),
    )
    .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
    .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE);

    let grpc_handle = tokio::spawn(async move {
        info!("🚀 gRPC 服务启动，监听端口: {}", grpc_port);
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(grpc_service)
            .serve(grpc_addr)
            .await
        {
            error!("gRPC 服务器错误: {}", e);
        }
    });

    // 启动 HTTP 服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .map_err(|e| anyhow::anyhow!("HTTP 服务器绑定端口 {} 失败: {}", config.port, e))?;

    info!("Server starting on port {}", config.port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent (HTTP, legacy)");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks");
    info!("  GET  /health - Health check");
    info!("gRPC endpoints (port {}):", grpc_port);
    info!("  agent.AgentService/Chat - gRPC chat");
    info!("  agent.AgentService/SubscribeProgress - gRPC progress stream");
    info!("  agent.AgentService/CancelSession - gRPC cancel");
    info!("  agent.AgentService/GetStatus - gRPC status");

    if let Some(proxy_config) = &config.proxy_config {
        info!("🚀 Pingora 反向代理服务已启用");
        info!("📡 监听端口: {}", proxy_config.listen_port);
        info!("🔄 路由格式: /proxy/{{port}}{{/path}} - 例如: /proxy/3000/api/users");
        info!("🌐 动态后端: 根据请求端口自动发现和代理后端服务");
        info!("💡 示例:");
        info!(
            "   http://localhost:{}/proxy/{}/health → http://127.0.0.1:{}/health",
            proxy_config.listen_port, config.port, config.port
        );
        info!(
            "   http://localhost:{}/proxy/{}/health → http://127.0.0.1:{}/health",
            proxy_config.listen_port, config.port, config.port
        );
        info!(
            "   http://localhost:{}/proxy/9000/health → http://127.0.0.1:9000/health (动态发现)",
            proxy_config.listen_port
        );
    }

    // 并行运行 HTTP 和 gRPC 服务
    tokio::select! {
        result = axum::serve(listener, app) => {
            if let Err(e) = result {
                error!("HTTP 服务器错误: {}", e);
            }
        }
        _ = grpc_handle => {
            warn!("gRPC 服务已停止");
        }
    }

    // 等待代理服务完成
    if let Some(handle) = proxy_handle {
        handle.await?;
    }

    Ok(())
}

/// 🔥 健康监控任务 (新架构)
///
/// 定期检查 Agent Worker 健康状态，自动重启不健康的 Worker
async fn spawn_health_monitor(runtime: Arc<AgentRuntime>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut consecutive_failures: u32 = 0;
        const MAX_RESTART_ATTEMPTS: u32 = 5;
        const RESTART_COOLDOWN_SECS: u64 = 60;

        info!("🔍 [HealthMonitor] 健康监控任务已启动");

        loop {
            interval.tick().await;

            // 检查健康状态
            if !runtime.check_health().await {
                error!("❌ [HealthMonitor] 检测到 Worker 不健康");

                // 检查冷却期
                if consecutive_failures >= MAX_RESTART_ATTEMPTS {
                    warn!(
                        "⏳ [HealthMonitor] 连续重启失败 {} 次，进入冷却期",
                        consecutive_failures
                    );
                    tokio::time::sleep(Duration::from_secs(RESTART_COOLDOWN_SECS)).await;
                    consecutive_failures = 0;
                    info!("🔄 [HealthMonitor] 冷却期结束，重置失败计数");
                }

                // 创建新的通道
                let (new_tx, new_rx) = tokio::sync::mpsc::channel(1000);

                // 重启 worker
                runtime.restart(new_rx).await;
                consecutive_failures += 1;
                info!("🔄 [HealthMonitor] Worker 重启完成（第 {} 次）", consecutive_failures);
            } else {
                consecutive_failures = 0;
            }
        }
    })
}
