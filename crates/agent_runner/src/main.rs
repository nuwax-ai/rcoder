use clap::Parser;
use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
use tracing_appender::rolling::Rotation;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod grpc;
mod handler;
mod model;
mod proxy_agent;

mod middleware;
mod router;
mod service;
mod utils;

use model::*;

use config::{CliArgs, load_config_with_args};
use pingora_proxy::{PingoraServerManager, ProxyConfig};
use proxy_agent::cleanup_task::{CleanupConfig, start_cleanup_task};
use router::AppState;

// 路由创建函数已移动到 handler 模块

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 初始化 OpenTelemetry
    init_telemetry()?;

    info!("Starting rcoder - AI-powered development platform");

    // 解析命令行参数
    let cli_args = CliArgs::parse();

    // 加载配置（包含命令行参数）
    let config = load_config_with_args(cli_args);

    // 创建项目工作目录
    tokio::fs::create_dir_all(&config.projects_dir).await?;
    info!("Projects directory: {:?}", config.projects_dir);

    // 创建本地任务通道
    let (local_task_sender, local_task_receiver) = tokio::sync::mpsc::unbounded_channel();

    // 创建清理配置
    let cleanup_config = CleanupConfig {
        idle_timeout: Duration::from_secs(3600),
        cleanup_interval: Duration::from_secs(30),
    };

    // 在主异步运行时中启动清理任务
    let _cleanup_handle = start_cleanup_task(cleanup_config.clone());

    // 在独立 OS 线程中启动单线程 tokio 运行时 + LocalSet，驻留运行 agent_worker（!Send）
    let _ = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build single-thread runtime for LocalSet agents");
        rt.block_on(async move {
            let local_set = tokio::task::LocalSet::new();
            local_set
                .run_until(async move {
                    // 运行 agent worker（cleanup task 已移到主线程）
                    if let Err(e) = proxy_agent::agent_worker(local_task_receiver).await {
                        error!("Failed to run agent worker: {}", e);
                    }
                    warn!("Agent worker stopped");
                })
                .await;
        });
    });

    // proxy_manager 不需要直接访问 app_state，通过参数传递即可

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

        // 创建 Pingora 服务器管理器，并提取服务引用用于指标读取
        let mut server_manager = PingoraServerManager::new(pingora_config);
        let pingora_service = server_manager.service();
        // 启动健康检查循环（按配置）
        if config.proxy_config.as_ref().unwrap().health_check.enabled {
            let hc = &config.proxy_config.as_ref().unwrap().health_check;
            pingora_service
                .start_health_check_loop(hc.interval_seconds, (hc.timeout_seconds * 1000) as u64);
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

    let state = Arc::new(AppState {
        sessions: Arc::new(DashMap::new()),
        config: config.clone(),
        local_task_sender,
        pingora_service: pingora_service_opt,
    });

    // 创建路由
    let app = router::create_router(state.clone());

    // 启动 gRPC 服务器
    let grpc_port = shared_types::GRPC_DEFAULT_PORT;
    let grpc_addr = format!("[::]:{}", grpc_port).parse().unwrap();
    let grpc_service = shared_types::grpc::agent_service_server::AgentServiceServer::new(
        grpc::AgentServiceImpl::new(state.clone()),
    );

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
        .unwrap();

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

    if config.proxy_config.is_some() {
        info!("🚀 Pingora 反向代理服务已启用");
        info!(
            "📡 监听端口: {}",
            config.proxy_config.as_ref().unwrap().listen_port
        );
        info!("🔄 路由格式: /proxy/{{port}}{{/path}} - 例如: /proxy/3000/api/users");
        info!("🌐 动态后端: 根据请求端口自动发现和代理后端服务");
        info!("💡 示例:");
        info!(
            "   http://localhost:{}/proxy/{}/health → http://127.0.0.1:{}/health",
            config.proxy_config.as_ref().unwrap().listen_port,
            config.port,
            config.port
        );
        info!(
            "   http://localhost:{}/proxy/{}/health → http://127.0.0.1:{}/health",
            config.proxy_config.as_ref().unwrap().listen_port,
            config.port,
            config.port
        );
        info!(
            "   http://localhost:{}/proxy/9000/health → http://127.0.0.1:9000/health (动态发现)",
            config.proxy_config.as_ref().unwrap().listen_port
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

/// 初始化遥测系统
fn init_telemetry() -> anyhow::Result<()> {
    // 创建 logs 目录（如果不存在）
    let logs_dir = Path::new("logs");
    if !logs_dir.exists() {
        std::fs::create_dir_all(logs_dir)?;
        info!("创建日志目录: {:?}", logs_dir);
    }

    // 设置按天滚动的文件 appender，保留最近5天的日志
    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("agent-runner")
        .max_log_files(5) // 保留最近5个日志文件
        .build(logs_dir)?;

    // 创建文件日志层 - JSON 格式，便于后续分析
    let file_layer = fmt::layer()
        .json()
        .with_writer(file_appender)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true);

    // 创建控制台日志层 - 简洁格式
    let console_layer = fmt::layer().with_target(false).with_ansi(true);

    // 设置全局文本传播器（用于 trace context 传播）
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    // 初始化 tracing subscriber，同时输出到文件和控制台
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "rcoder=debug,tower_http=debug,axum_tracing_opentelemetry=info".into()
            }),
        )
        .with(file_layer)
        .with(console_layer)
        .init();

    info!("✓ Tracing 初始化成功，支持 trace_id 生成和传播");
    info!("✓ 日志文件将按天滚动保存在 {:?} 目录", logs_dir);

    Ok(())
}
