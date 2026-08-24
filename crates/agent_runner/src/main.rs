use clap::Parser;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
#[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
use tracing::error;
use tracing::{info, warn};

// 🆕 使用共享的遥测模块
use rcoder_telemetry::{TelemetryConfig, TelemetryGuard};

pub use agent_runner::model::*;

use agent_runner::config::{CliArgs, load_config_with_args};
use agent_runner::proxy_agent::cleanup_task::{CleanupConfig, start_cleanup_task};
#[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
use agent_runner::router::AppState;
use agent_runner::service::AgentSessionService;
use agent_runner::shutdown::{set_panic_hook, setup_shutdown_handler};
use agent_runner::utils::spawn_tool_version_log;

fn create_model_env_resolver(
    config: &agent_runner::config::AppConfig,
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

fn main() -> anyhow::Result<()> {
    // 容器内若为 PID 1(被 start-up.sh exec / 直跑),re-exec 自己为子进程 + 本进程做 PID 1 监督
    // (回收孤儿 + 转发 SIGTERM/SIGINT)。等价 tini,但纯 Rust 库、无需镜像装 tini 或命令前置。
    // 关键:监督进程的 waitpid(-1) 与 app 的 tokio::process 在 **不同进程**(app 是 PID 2 子进程),
    // 不再像旧 in-process process_reaper 那样抢 tokio 的子进程 → ECHILD 彻底消失。
    // 非 PID 1(本地直接跑)launch() 直接返回,行为不变。
    pid1::Pid1Settings::new()
        .enable_log(true)
        .timeout(Duration::from_secs(15))
        .launch()?;
    agent_runner_main()
}

#[tokio::main]
async fn agent_runner_main() -> anyhow::Result<()> {
    // 🔥 设置自定义 Panic Hook，确保 panic 信息被记录
    set_panic_hook();

    // 🔥 设置信号处理器，实现优雅关闭（Docker stop、Ctrl+C）
    setup_shutdown_handler();

    // ✅ 初始化 Rustls CryptoProvider（必须在最前面，在任何可能使用 TLS 的代码之前）
    // 🔥 如果这里失败，会导致 panic，但 panic hook 会捕获并记录
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect(
            "❌ [FATAL] Rustls CryptoProvider initialization failed. The process cannot continue. This is usually an environment issue.",
        );

    // 🆕 Initializing telemetry system（使用 rcoder-telemetry，包含控制台 + 文件日志）
    let telemetry_config = TelemetryConfig::from_env("agent_runner").with_file_log("agent-runner"); // 启用文件日志，前缀为 agent-runner
    // tokio-console 观测（console feature；shadowing 绑定——无 feature 时零代码）
    #[cfg(feature = "console")]
    let telemetry_config = agent_runner::console_obs::attach(telemetry_config);

    let telemetry: TelemetryGuard = rcoder_telemetry::init(telemetry_config).await?;
    let _telemetry = Arc::new(telemetry);

    // 初始化 FeatureFlags（进程级单例：读 RCODER_EMBED_FILE_SERVER 等环境开关）
    shared_types::FeatureFlags::init();

    // 打印 tokio runtime worker 数, 排查"cpu limit 是否导致 worker=1 阻塞"。
    // tokio multi_thread 默认 worker = 物理核数 (num_cpus), 不受 cgroup CFS quota 影响;
    // 仅 cpuset 静态绑核才受限 → cpu limit=1 不会让 worker=1。
    info!(
        "tokio runtime workers: {}",
        tokio::runtime::Handle::current().metrics().num_workers()
    );

    // 🆕 Pyroscope Profiler 初始化（可选：需要 pyroscope feature）
    #[cfg(feature = "pyroscope")]
    let _pyroscope_guard: Option<agent_runner::profiler::ProfilerGuard> = {
        info!("Pyroscope profiling feature enabled");
        match agent_runner::profiler::init_pyroscope_profiler_default() {
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
    info!("agent-runner version: {}", env!("CARGO_PKG_VERSION"));

    // 非阻塞打印外部工具版本（不阻塞启动流程）
    spawn_tool_version_log("nuwaxcode", &["nuwaxcode", "-v"]);
    spawn_tool_version_log(
        shared_types::DEFAULT_AGENT_ID,
        &[shared_types::DEFAULT_AGENT_ID, "-v"],
    );

    // 异步初始化内置 agent 版本缓存（不阻塞主流程）
    tokio::spawn(async {
        agent_runner::agent_mgmt::checker::init_builtin_agent_versions().await;
    });

    // 解析命令行参数
    let cli_args = CliArgs::parse();

    // 加载配置（包含命令行参数）
    let config = load_config_with_args(cli_args);

    // 注:容器以 tini 做 PID 1(见镜像 ENTRYPOINT / chart command 前置 tini),由 tini 负责回收
    // 孤儿进程 + 转发信号。agent_runner 不再自带 in-process reaper(旧 process_reaper 模块已删:
    // 它的 waitpid(-1) 会抢 tokio::process 的子进程导致 build child.wait() ECHILD)。

    // 🆕 从配置中获取 Agent 清理配置，或使用默认值
    let agent_cleanup_config = config.agent_cleanup.clone().unwrap_or_default();
    let cleanup_config = CleanupConfig {
        idle_timeout: Duration::from_secs(agent_cleanup_config.idle_timeout_secs),
        cleanup_interval: Duration::from_secs(agent_cleanup_config.cleanup_interval_secs),
    };

    info!(
        "[MAIN] Agent cleanup config: idle_timeout={}s, cleanup_interval={}s",
        agent_cleanup_config.idle_timeout_secs, agent_cleanup_config.cleanup_interval_secs
    );

    // 在主异步运行时中启动清理任务
    let _cleanup_handle = start_cleanup_task(cleanup_config.clone());

    // proxy_manager 不需要直接访问 app_state，通过参数传递即可

    // 🔒 创建共享的 API 密钥 DashMap
    let shared_api_key_manager =
        Arc::new(DashMap::<String, shared_types::ModelProviderConfig>::new());
    info!("[MAIN] Shared API key DashMap created");

    #[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
    let api_key_manager = Arc::new(agent_runner::api_key_manager::ApiKeyManager::from_shared(
        shared_api_key_manager.clone(),
    ));

    // 跨协议共享的 project_id → service_uuid 映射：gRPC 域创建（chat 路径
    // insert），HTTP 域注入同一实例——两份独立 map 会让 gRPC StopAgent 找不到
    // HTTP 域写入的映射，shared_api_key_manager 中该 uuid 的 api_key 永不清理
    #[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
    let project_uuid_map: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    // HTTP 侧注入视图：grpc 双开时共享；http-only 时 None（AppState 自建，
    // 单协议形态无跨协议清理需求）
    #[allow(unused_variables)]
    let http_shared_uuid_map: Option<Arc<DashMap<String, String>>> = {
        #[cfg(any(feature = "grpc-server", not(feature = "http-server")))]
        {
            Some(project_uuid_map.clone())
        }
        #[cfg(all(not(feature = "grpc-server"), feature = "http-server"))]
        {
            None
        }
    };

    let model_env_resolver: Arc<dyn agent_abstraction::launcher::ModelRuntimeEnvResolver> =
        create_model_env_resolver(&config);
    // ACP session 创建超时：取自 GrpcTimeoutConfig（已 validate ∈ [10,300]，默认 100），
    // 经 AgentSessionService → AcpAgentWorker → AgentStartConfig 注入到 launcher 外层超时。
    let acp_session_create_timeout_secs = config
        .grpc_timeouts
        .as_ref()
        .map(|t| t.acp_session_create_timeout_secs)
        .unwrap_or(100);
    let agent_session_service = Arc::new(AgentSessionService::new(
        model_env_resolver,
        acp_session_create_timeout_secs,
    ));
    info!("[MAIN] AgentSessionService created");

    // 🆕 P0-1: 创建 Agent 管理注册表(从磁盘加载,失败则用空注册表 + 警告)
    let agent_mgmt_path_manager = agent_runner::agent_mgmt::PathManager::new();
    let agent_mgmt_registry =
        match agent_runner::agent_mgmt::AgentRegistry::load(agent_mgmt_path_manager.clone()) {
            Ok(r) => {
                info!(
                    "[MAIN] Agent management registry loaded: total={}, builtin={}",
                    r.total(),
                    r.builtin_count()
                );
                Arc::new(r)
            }
            Err(e) => {
                tracing::warn!(
                    "[MAIN] Failed to load agent management registry, starting empty: {e}"
                );
                Arc::new(agent_runner::agent_mgmt::AgentRegistry::empty(
                    agent_mgmt_path_manager.clone(),
                ))
            }
        };

    // 🔥 http-server 模式：启动 HTTP + (可选 gRPC) + Pingora
    #[cfg(feature = "http-server")]
    {
        use agent_runner::http_server::{HttpServerConfig, start_http_server};
        // 🔥 1. 可选：启动 gRPC 服务（当 grpc-server feature 启用时）
        #[cfg(feature = "grpc-server")]
        let grpc_handle = {
            info!("HTTP server mode: starting HTTP + gRPC + Pingora");

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
                agent_mgmt_registry: agent_mgmt_registry.clone(),
                agent_mgmt_path_manager: agent_mgmt_path_manager.clone(),
            });

            // gRPC 消息大小限制
            let grpc_service = shared_types::grpc::agent_service_server::AgentServiceServer::new(
                agent_runner::grpc::AgentServiceImpl::new(grpc_state.clone()),
            )
            .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
            .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE);

            // P0-1: Agent 管理 gRPC 服务
            let agent_mgmt_service = agent_runner::agent_mgmt::grpc::AgentMgmtServiceImpl::new(
                agent_mgmt_registry.clone(),
                agent_mgmt_path_manager.clone(),
            );

            let handle = tokio::spawn(async move {
                info!("gRPC service started, listening on port: {}", grpc_port);
                info!("gRPC endpoints (port {}):", grpc_port);
                info!("  agent.AgentService/Chat - gRPC chat");
                info!("  agent.AgentService/SubscribeProgress - gRPC progress stream");
                info!("  agent.AgentService/CancelSession - gRPC cancel");
                info!("  agent.AgentService/GetStatus - gRPC status");
                info!("  agent.AgentMgmtService/* - agent management (P0-1)");
                if let Err(e) = tonic::transport::Server::builder()
                    .add_service(grpc_service)
                    .add_service(
                        shared_types::grpc::agent_mgmt_service_server::AgentMgmtServiceServer::new(
                            agent_mgmt_service,
                        )
                        .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
                        .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE),
                    )
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
            info!("HTTP server mode: starting HTTP + Pingora only (no gRPC)");
        }

        // 🔥 1.5. 启动 ttyd WS 终端中间层（tokio-tungstenite：接浏览器 + 连本地 ttyd）
        //         cd 逻辑由代码每次连接（含重连）控制，解决 WS 重连不进项目目录的问题
        tokio::spawn(async move {
            agent_runner::ws_terminal::start_ws_terminal().await;
        });

        // 🔥 1.6. 可选：内嵌 file-server (RCODER_EMBED_FILE_SERVER=true)
        //         路由 merge 进 8086 主服务（create_router 内按开关注入）；
        //         60000 由 file-server-proxy 前置接管（AllRust → 本进程 8086）
        if shared_types::FeatureFlags::get().embed_file_server {
            agent_runner::file_server_embed::spawn_file_server_proxy(config.port).await;
        }

        // 🔥 2. 创建 HttpServerConfig（包含所有配置）
        let http_config = HttpServerConfig {
            port: config.port,
            app_config: config.clone(),
            agent_session_service: agent_session_service.clone(),
            shared_api_key_manager: shared_api_key_manager.clone(),
            project_uuid_map: http_shared_uuid_map.clone(),
            agent_mgmt_registry: Some(agent_mgmt_registry.clone()),
            agent_mgmt_path_manager: Some(agent_mgmt_path_manager.clone()),
        };

        // 🔥 3. 启动 HTTP 服务器（内部会启动 Pingora）
        let _handle = start_http_server(http_config).await?;

        // 🔥 4. 同时等待 gRPC（如果有）和信号
        info!("HTTP + Pingora services started; running until shutdown signal is received");

        #[cfg(feature = "grpc-server")]
        {
            match grpc_handle {
                Some(handle) => {
                    tokio::select! {
                        result = handle => {
                            match result {
                                Ok(_) => info!("gRPC service ended normally"),
                                Err(e) if e.is_panic() => {
                                    error!("gRPC service panicked: {:?}", e);
                                }
                                Err(e) if e.is_cancelled() => {
                                    info!("gRPC service was cancelled");
                                }
                                Err(e) => {
                                    error!("gRPC service ended with error: {:?}", e);
                                }
                            }
                        }
                        _ = tokio::signal::ctrl_c() => {
                            info!("Received shutdown signal, preparing graceful shutdown...");
                        }
                    }
                }
                None => {
                    // This should never happen if grpc-server feature is enabled
                    error!(
                        "CRITICAL: gRPC handle is None despite grpc-server feature being enabled. This is a bug in initialization logic."
                    );
                    // Wait for ctrl_c instead of silently continuing
                    tokio::signal::ctrl_c().await?;
                    info!("Received shutdown signal, preparing graceful shutdown...");
                }
            }
        }

        #[cfg(not(feature = "grpc-server"))]
        {
            tokio::signal::ctrl_c().await?;
            info!("Received shutdown signal, preparing graceful shutdown...");
        }

        Ok(())
    }

    // 🔥 non-http-server 模式：启动 gRPC + Pingora（用于 Docker 容器内）
    #[cfg(not(feature = "http-server"))]
    {
        info!("Container mode: starting gRPC + Pingora");

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
            agent_mgmt_registry: agent_mgmt_registry.clone(),
            agent_mgmt_path_manager: agent_mgmt_path_manager.clone(),
        });

        // gRPC 消息大小限制
        let grpc_service = shared_types::grpc::agent_service_server::AgentServiceServer::new(
            agent_runner::grpc::AgentServiceImpl::new(grpc_state.clone()),
        )
        .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
        .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE);

        // P0-1: Agent 管理 gRPC 服务
        let agent_mgmt_service = agent_runner::agent_mgmt::grpc::AgentMgmtServiceImpl::new(
            agent_mgmt_registry.clone(),
            agent_mgmt_path_manager.clone(),
        );

        let grpc_handle = tokio::spawn(async move {
            info!("gRPC service started, listening on port: {}", grpc_port);
            info!("gRPC endpoints (port {}):", grpc_port);
            info!("  agent.AgentService/Chat - gRPC chat");
            info!("  agent.AgentService/SubscribeProgress - gRPC progress stream");
            info!("  agent.AgentService/CancelSession - gRPC cancel");
            info!("  agent.AgentService/GetStatus - gRPC status");
            info!("  agent.AgentMgmtService/* - agent management (P0-1)");
            if let Err(e) = tonic::transport::Server::builder()
                .add_service(grpc_service)
                .add_service(
                    shared_types::grpc::agent_mgmt_service_server::AgentMgmtServiceServer::new(
                        agent_mgmt_service,
                    )
                    .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
                    .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE),
                )
                .serve(grpc_addr)
                .await
            {
                error!("gRPC server error: {}", e);
            }
        });

        // 启动轻量 HTTP 健康检查服务（供 docker_manager 健康检查使用）
        let health_port = config.port; // 默认 8086，来自 --port 参数
        let _health_handle = tokio::spawn(async move {
            use agent_runner::handler::health_handler::{
                build_health_response, check_grpc_port_simple,
            };
            use axum::{Json, Router, routing::get};

            async fn health_check()
            -> Json<shared_types::HttpResult<shared_types::HealthCheckResponse>> {
                // HTTP 服务：本端点正常响应即表示就绪
                let http_ready = true;

                // 检查 gRPC 端口是否就绪
                let grpc_ready = check_grpc_port_simple().await;

                // 使用统一的健康检查响应构建函数
                Json(build_health_response(
                    "agent-runner",
                    http_ready,
                    grpc_ready,
                ))
            }

            let app = Router::new().route("/health", get(health_check));
            let addr = format!("0.0.0.0:{}", health_port);

            info!(
                "HTTP health check service started, listening on port: {}",
                health_port
            );

            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!(
                        "Failed to bind HTTP health check service: {} (port: {})",
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
            use agent_runner::proxy_agent::start_pingora;

            if let Some(proxy_config) = &config.proxy_config {
                Some(start_pingora(proxy_config, shared_api_key_manager.clone())?)
            } else {
                info!("Pingora proxy service is not configured");
                None
            }
        };

        #[cfg(not(feature = "proxy"))]
        let pingora_result: Option<()> = {
            info!("Pingora proxy service is disabled (proxy feature not enabled)");
            None
        };

        // 等待 gRPC 服务
        if let Err(e) = grpc_handle.await {
            if e.is_panic() {
                error!("gRPC service panicked: {:?}", e);
            } else if e.is_cancelled() {
                info!("gRPC service was cancelled");
            } else {
                error!("gRPC service ended with error: {:?}", e);
            }
        }

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
