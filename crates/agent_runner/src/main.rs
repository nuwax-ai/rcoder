use clap::Parser;
#[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

// 🆕 使用共享的遥测模块
use rcoder_telemetry::{TelemetryConfig, TelemetryGuard};

mod api_key_manager;
mod config;
#[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
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
mod shutdown;
mod utils;

// HTTP 服务器模块 (仅在 http-server feature 启用时)
#[cfg(feature = "http-server")]
mod http_server;

pub use model::*;

use config::{CliArgs, load_config_with_args};
use proxy_agent::cleanup_task::{CleanupConfig, start_cleanup_task};
#[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
use router::AppState;
use service::AgentSessionService;
use shutdown::{set_panic_hook, setup_shutdown_handler};

fn create_model_env_resolver(
    config: &config::AppConfig,
) -> Arc<dyn agent_abstraction::launcher::ModelRuntimeEnvResolver> {
    #[cfg(feature = "proxy")]
    {
        if let Some(proxy_config) = &config.proxy_config {
            let proxy_base_url_template = format!(
                "http://localhost:{}/api/{{SERVICE_UUID}}",
                proxy_config.listen_port
            );
            info!(
                "🔒 [MAIN] Proxy model env enabled: {}",
                proxy_base_url_template
            );
            return Arc::new(
                agent_abstraction::launcher::ProxyModelRuntimeEnvResolver::new(
                    proxy_base_url_template,
                ),
            );
        }
    }

    #[cfg(not(feature = "proxy"))]
    if config.proxy_config.is_some() {
        warn!("Proxy config is present, but proxy feature is not enabled; using direct model env");
    }

    Arc::new(agent_abstraction::launcher::DirectModelRuntimeEnvResolver)
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
        .expect(
            "❌ [FATAL] Rustls CryptoProvider initialization failed. The process cannot continue. This is usually an environment issue.",
        );

    // 🆕 Initializing telemetry system（使用 rcoder-telemetry，包含控制台 + 文件日志）
    let telemetry_config = TelemetryConfig::from_env("agent_runner").with_file_log("agent-runner"); // 启用文件日志，前缀为 agent-runner
    let telemetry: TelemetryGuard = rcoder_telemetry::init(telemetry_config).await?;
    let _telemetry = Arc::new(telemetry);

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

    // 🔥 启动僵尸进程回收器（PID 1 必须回收孤儿进程）
    let _reaper_handle = process_reaper::start_process_reaper();
    info!("🧹 [MAIN] Process reaper started (PID 1 mode)");

    // 🆕 从配置中获取 Agent 清理配置，或使用默认值
    let agent_cleanup_config = config.agent_cleanup.clone().unwrap_or_default();
    let cleanup_config = CleanupConfig {
        idle_timeout: Duration::from_secs(agent_cleanup_config.idle_timeout_secs),
        cleanup_interval: Duration::from_secs(agent_cleanup_config.cleanup_interval_secs),
    };

    info!(
        "🧹 [MAIN] Agent cleanup config: idle_timeout={}s, cleanup_interval={}s",
        agent_cleanup_config.idle_timeout_secs, agent_cleanup_config.cleanup_interval_secs
    );

    // 在主异步运行时中启动清理任务
    let _cleanup_handle = start_cleanup_task(cleanup_config.clone());

    // proxy_manager 不需要直接访问 app_state，通过参数传递即可

    // 🔒 创建共享的 API 密钥 DashMap
    let shared_api_key_manager =
        Arc::new(dashmap::DashMap::<String, shared_types::ModelProviderConfig>::new());
    info!("🔑 [MAIN] Shared API key DashMap created");

    #[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
    let api_key_manager = Arc::new(api_key_manager::ApiKeyManager::from_shared(
        shared_api_key_manager.clone(),
    ));

    #[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
    let project_uuid_map: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let model_env_resolver: Arc<dyn agent_abstraction::launcher::ModelRuntimeEnvResolver> =
        create_model_env_resolver(&config);
    let agent_session_service = Arc::new(AgentSessionService::new(model_env_resolver));
    info!("🔧 [MAIN] AgentSessionService created");

    // 🔥 http-server 模式：启动 HTTP + (可选 gRPC) + Pingora
    #[cfg(feature = "http-server")]
    {
        use http_server::{HttpServerConfig, start_http_server};
        // 🔥 1. 可选：启动 gRPC 服务（当 grpc-server feature 启用时）
        #[cfg(feature = "grpc-server")]
        let grpc_handle = {
            info!("ℹ️  HTTP server mode: starting HTTP + gRPC + Pingora");

            let grpc_port = shared_types::GRPC_DEFAULT_PORT;
            let grpc_addr = format!("[::]:{}", grpc_port)
                .parse()
                .map_err(|e| anyhow::anyhow!("Failed to parse gRPC address: {}", e))?;

            // 为 gRPC 创建 state
            let grpc_state = Arc::new(AppState {
                sessions: Arc::new(DashMap::new()),
                config: config.clone(),
                agent_session_service: agent_session_service.clone(),
                #[cfg(feature = "proxy")]
                pingora_service: None,
                api_key_manager: api_key_manager.clone(),
                shared_api_key_manager: shared_api_key_manager.clone(),
                project_uuid_map: project_uuid_map.clone(),
            });

            // gRPC 消息大小限制
            let grpc_service = shared_types::grpc::agent_service_server::AgentServiceServer::new(
                grpc::AgentServiceImpl::new(grpc_state.clone()),
            )
            .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
            .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE);

            let handle = tokio::spawn(async move {
                info!("gRPC service started, listening on port: {}", grpc_port);
                info!("gRPC endpoints (port {}):", grpc_port);
                info!("  agent.AgentService/Chat - gRPC chat");
                info!("  agent.AgentService/SubscribeProgress - gRPC progress stream");
                info!("  agent.AgentService/CancelSession - gRPC cancel");
                info!("  agent.AgentService/GetStatus - gRPC status");
                if let Err(e) = tonic::transport::Server::builder()
                    .add_service(grpc_service)
                    .serve(grpc_addr)
                    .await
                {
                    error!("gRPC server error: {}", e);
                }
            });

            Some(handle)
        };

        // 无 gRPC 模式
        #[cfg(not(feature = "grpc-server"))]
        {
            info!("ℹ️  HTTP server mode: starting HTTP + Pingora only (no gRPC)");
        }

        // 🔥 2. 创建 HttpServerConfig（包含所有配置）
        let http_config = HttpServerConfig {
            port: config.port,
            app_config: config.clone(),
            agent_session_service: agent_session_service.clone(),
            shared_api_key_manager: shared_api_key_manager.clone(),
        };

        // 🔥 3. 启动 HTTP 服务器（内部会启动 Pingora）
        let _handle = start_http_server(http_config).await?;

        // 🔥 4. 同时等待 gRPC（如果有）和信号
        info!("HTTP + Pingora services started; running until shutdown signal is received");

        #[cfg(feature = "grpc-server")]
        {
            tokio::select! {
                _ = grpc_handle.unwrap() => {
                    info!("gRPC service ended unexpectedly, shutting down...");
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("📨 Received shutdown signal, preparing graceful shutdown...");
                }
            }
        }

        #[cfg(not(feature = "grpc-server"))]
        {
            tokio::signal::ctrl_c().await?;
            info!("📨 Received shutdown signal, preparing graceful shutdown...");
        }

        Ok(())
    }

    // 🔥 non-http-server 模式：启动 gRPC + Pingora（用于 Docker 容器内）
    #[cfg(not(feature = "http-server"))]
    {
        info!("ℹ️  Container mode: starting gRPC + Pingora");

        // 启动 gRPC 服务
        let grpc_port = shared_types::GRPC_DEFAULT_PORT;
        let grpc_addr = format!("[::]:{}", grpc_port)
            .parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse gRPC address: {}", e))?;

        // 为 gRPC 创建 state
        let grpc_state = Arc::new(AppState {
            sessions: Arc::new(DashMap::new()),
            config: config.clone(),
            agent_session_service: agent_session_service.clone(),
            #[cfg(feature = "proxy")]
            pingora_service: None,
            api_key_manager: api_key_manager.clone(),
            shared_api_key_manager: shared_api_key_manager.clone(),
            project_uuid_map: project_uuid_map.clone(),
        });

        // gRPC 消息大小限制
        let grpc_service = shared_types::grpc::agent_service_server::AgentServiceServer::new(
            grpc::AgentServiceImpl::new(grpc_state.clone()),
        )
        .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
        .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE);

        let grpc_handle = tokio::spawn(async move {
            info!("gRPC service started, listening on port: {}", grpc_port);
            info!("gRPC endpoints (port {}):", grpc_port);
            info!("  agent.AgentService/Chat - gRPC chat");
            info!("  agent.AgentService/SubscribeProgress - gRPC progress stream");
            info!("  agent.AgentService/CancelSession - gRPC cancel");
            info!("  agent.AgentService/GetStatus - gRPC status");
            if let Err(e) = tonic::transport::Server::builder()
                .add_service(grpc_service)
                .serve(grpc_addr)
                .await
            {
                error!("gRPC server error: {}", e);
            }
        });

        // 启动轻量 HTTP 健康检查服务（供 docker_manager 健康检查使用）
        let health_port = config.port; // 默认 8086，来自 --port 参数
        let _health_handle = tokio::spawn(async move {
            use axum::{Json, Router, routing::get};

            async fn health_check() -> Json<shared_types::HealthResponse> {
                Json(shared_types::HealthResponse::new("agent-runner"))
            }

            let app = Router::new().route("/health", get(health_check));
            let addr = format!("0.0.0.0:{}", health_port);

            info!(
                "🏥 HTTP health check service started, listening on port: {}",
                health_port
            );

            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!(
                        "❌ Failed to bind HTTP health check service: {} (port: {})",
                        e, health_port
                    );
                    return;
                }
            };

            if let Err(e) = axum::serve(listener, app).await {
                error!("HTTP health check service error: {}", e);
            }
        });

        // 启动 Pingora（如有配置且启用了 proxy feature）
        #[cfg(feature = "proxy")]
        let pingora_result = {
            use proxy_agent::start_pingora;

            if let Some(proxy_config) = &config.proxy_config {
                Some(start_pingora(proxy_config, shared_api_key_manager.clone())?)
            } else {
                info!("ℹ️  Pingora proxy service is not configured");
                None
            }
        };

        #[cfg(not(feature = "proxy"))]
        let pingora_result: Option<()> = {
            info!("ℹ️  Pingora proxy service is disabled (proxy feature not enabled)");
            None
        };

        // 等待 gRPC 服务
        let _ = grpc_handle.await;

        // 停止 Pingora 服务
        #[cfg(feature = "proxy")]
        if let Some(mut result) = pingora_result {
            result.stop().await;
        }

        #[cfg(not(feature = "proxy"))]
        let _ = pingora_result;

        Ok(())
    }
}
