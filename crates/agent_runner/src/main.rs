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

// 路由创建函数已移动到 handler 模块

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ✅ 初始化 Rustls CryptoProvider（必须在最前面，在任何可能使用 TLS 的代码之前）
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

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
