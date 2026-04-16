use arc_swap::ArcSwap;
use clap::Parser;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{error, info, warn};

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;

// 🆕 使用共享的遥测模块
use rcoder_telemetry::{TelemetryConfig, TelemetryGuard};

mod config;
mod config_watcher;
mod handler;

mod cleanup_task;
mod middleware;
mod router;
mod service;
mod utils;

use rcoder::*;

use config::{CliArgs, load_config_with_args};
use rcoder_proxy::{PingoraServerManager, ProxyConfig};
use router::AppState;
use service::{
    ContainerStatusCheckerConfig, ContainerSyncConfig, VncSyncConfig,
    start_container_status_checker, start_container_sync_task, start_vnc_sync_task,
};

// 导入统一的容器停止模块
use docker_manager::container_stop;
use docker_manager::runtime_selection::RuntimeType;

// 路由创建函数已移动到 handler 模块

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ✅ 初始化 Rustls CryptoProvider（必须在最前面，在任何可能使用 TLS 的代码之前）
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // 解析命令行参数（移到最前面，以便尽早加载配置）
    let cli_args = CliArgs::parse();

    // 加载配置（包含命令行参数）
    let config = load_config_with_args(cli_args)?;

    // 🆕 Initializing telemetry system（使用 rcoder-telemetry，包含控制台 + 文件日志）
    // 使用配置文件中的日志保留天数，与容器日志清理保持一致
    let file_log_config = rcoder_telemetry::FileLogConfig::new("logs", "rcoder")
        .with_max_files(config.cleanup_config.log_cleanup.log_retention_days as usize);

    let telemetry_config =
        TelemetryConfig::from_env("rcoder").with_file_log_config(file_log_config);
    let telemetry: TelemetryGuard = rcoder_telemetry::init(telemetry_config).await?;
    let telemetry = Arc::new(telemetry);

    info!("Starting rcoder - AI-powered development platform");
    info!(
        "📋 Log config: keeping log files for {} days",
        config.cleanup_config.log_cleanup.log_retention_days
    );

    // 创建项目工作目录
    tokio::fs::create_dir_all(&config.projects_dir).await?;
    info!("Projects directory: {:?}", config.projects_dir);

    // 🔄 初始化宿主机路径解析器（自动检测模式）
    info!("starting to detect mount path...");
    let docker_socket_path = std::env::var("DOCKER_SOCKET_PATH").unwrap_or_else(|_| {
 info!("DOCKER_SOCKET_PATH not set, using default: /var/run/docker.sock");
        "/var/run/docker.sock".to_string()
    });

    info!("Docker socket: {}", docker_socket_path);

    let _path_resolver =
        match utils::HostPathResolver::new_with_docker_socket(Some(docker_socket_path.clone()))
            .await
        {
            Ok(resolver) => {
                info!("path resolver initialized successfully");
                info!(
                    "  Container workspace: {:?}",
                    resolver.container_workspace_base()
                );
                info!(
                    "work directory: {:?}",
                    resolver.host_workspace_base()
                );
                Some(resolver)
            }
            Err(e) => {
                error!("path resolver initialization failed: {}", e);
                error!("please check config:");
                error!("1. Docker socket path: {}", docker_socket_path);
                error!("2. Docker socket already mounted in container");
                error!("3. container has Docker API access");
                error!("4. project work directory mounted");

                // 显示详细的错误信息和解决建议
                show_docker_configuration_help(&docker_socket_path);

                // 返回错误，停止启动
                return Err(anyhow::anyhow!("Container self-check failed, unable to initialize path resolver"));
            }
        };

    // 🧹 启动时清理上次可能遗留的容器
    // 先使用正确配置初始化全局 DockerManager
    info!("initialize Docker Manager (with config)...");

    // 从应用配置创建 DockerManagerConfig
    let docker_manager_config = if let Some(docker_config) = &config.docker_config {
        info!("using Docker config, merging config");
        let mut default_config = docker_manager::DockerManagerConfig::default();

        // 合并应用配置中的多镜像配置
        let app_multi_config = docker_config.get_multi_image_config();
        default_config.multi_image_config = app_multi_config;

        // 应用其他配置
        default_config.auto_cleanup = docker_config
            .auto_cleanup
            .unwrap_or(default_config.auto_cleanup);
        if let Some(ttl) = docker_config.container_ttl_seconds {
            default_config.container_ttl_seconds = Some(ttl);
        }

        // 应用网络基础名称配置
        info!(
            "🔍 [DEBUG] docker_config.network_base_name = {:?}",
            docker_config.network_base_name
        );
        if let Some(ref network_base_name) = docker_config.network_base_name {
            info!("using config: {}", network_base_name);
            default_config.network_base_name = network_base_name.clone();
        } else {
            info!(
                "⚠️ No network_base_name in config, using default: {}",
                default_config.network_base_name
            );
        }

        // 🔧 应用超时配置
        if let Some(timeout) = docker_config.api_timeout_seconds {
            default_config.api_timeout_seconds = timeout;
            info!("using config: API timeout: {} seconds", timeout);
        }
        if let Some(timeout) = docker_config.api_timeout_quick_seconds {
            default_config.api_timeout_quick_seconds = timeout;
            info!("using config: timeout: {} seconds", timeout);
        }

        // 🔧 应用缓存 TTL 配置
        if let Some(ttl) = docker_config.cache_status_ttl_seconds {
            default_config.cache_status_ttl_seconds = ttl;
            info!(
                "using config: status cache TTL: {} seconds",
                ttl
            );
        }
        if let Some(ttl) = docker_config.cache_network_ttl_seconds {
            default_config.cache_network_ttl_seconds = ttl;
            info!("using config: network cache TTL: {} seconds", ttl);
        }

        default_config
    } else {
        info!("⚠️ no Docker config, using default config");
        docker_manager::DockerManagerConfig::default()
    };

    // 使用自定义配置初始化全局 DockerManager
    if let Err(e) =
        docker_manager::global::init_global_docker_manager_with_config(docker_manager_config).await
    {
        error!("Docker Manager initializefailed: {}", e);
        return Err(anyhow::anyhow!("Docker Manager initialization failed: {}", e));
    }

    info!("checking cleanup for container (enabled)...");
    if config.cleanup_config.enabled {
        match docker_manager::runtime::RuntimeManager::runtime_type() {
            RuntimeType::Docker => {
                let docker_manager = match docker_manager::global::get_global_docker_manager().await {
                    Ok(dm) => {
                        info!("Docker Manager initialized successfully (with config)");
                        dm
                    }
                    Err(e) => {
                        error!("get Docker Manager failed: {}", e);
                        return Err(anyhow::anyhow!("Failed to get Docker Manager: {}", e));
                    }
                };

                let multi_image_config = if let Some(docker_config) = &config.docker_config {
                    docker_config.get_multi_image_config()
                } else {
                    shared_types::create_default_multi_image_config()
                };

                match container_stop::startup_cleanup_all_enabled_services(
                    &docker_manager,
                    &multi_image_config,
                )
                .await
                {
                    Ok(result) => {
                        let enabled_services = shared_types::get_enabled_service_types(&multi_image_config);
                        if result.successfully_removed > 0 {
                            info!(
                                "✅ Startup cleanup completed, removed {} leftover containers (covering {} service types)",
                                result.successfully_removed,
                                enabled_services.len()
                            );
                        } else {
                            info!("no containers to cleanup");
                        }

                        if result.failed_removals > 0 {
                            warn!(
                                "container cleanup failed: failed count={}",
                                result.failed_removals
                            );
                            for failure in &result.failed_removals_details {
                                warn!(
                                    "  - Container {} ({}): {}",
                                    failure.container_id, failure.container_name, failure.error_message
                                );
                            }
                        }
                    }
                    Err(e) => {
                        warn!("container cleanup failed: {}, cleanup skipped", e);
                    }
                }
            }
            RuntimeType::Kubernetes => {
                match docker_manager::runtime::RuntimeManager::get().await {
                    Ok(runtime) => {
                        if let Err(e) = runtime.cleanup_all().await {
                            warn!("k8s startup cleanup failed: {}", e);
                        } else {
                            info!("k8s startup cleanup completed");
                        }
                    }
                    Err(e) => warn!("failed to get runtime for k8s startup cleanup: {}", e),
                }
            }
        }
    } else {
        info!("Container cleanup task already started (cleanup_config.enabled=false)");
    }

    // 从配置文件读取清理配置
    let cleanup_config = cleanup_task::CleanupConfig {
        idle_timeout: Duration::from_secs(config.cleanup_config.idle_timeout_seconds),
        cleanup_interval: Duration::from_secs(config.cleanup_config.cleanup_interval_seconds),
        docker_stop_timeout: Duration::from_secs(config.cleanup_config.docker_stop_timeout_seconds),
        container_protection_duration: Duration::from_secs(
            config.cleanup_config.container_protection_seconds,
        ),
        active_window: Duration::from_secs(5 * 60),
        log_dir: config.cleanup_config.log_cleanup.log_dir.clone(),
        log_retention_duration: Duration::from_secs(
            config.cleanup_config.log_cleanup.log_retention_days * 24 * 60 * 60,
        ),
    };
    info!(
        "🧹 Cleanup config: idle_timeout={}s, cleanup_interval={}s, docker_stop_timeout={}s, container_protection={}s, log_dir={}, log_retention={}days",
        config.cleanup_config.idle_timeout_seconds,
        config.cleanup_config.cleanup_interval_seconds,
        config.cleanup_config.docker_stop_timeout_seconds,
        config.cleanup_config.container_protection_seconds,
        config.cleanup_config.log_cleanup.log_dir,
        config.cleanup_config.log_cleanup.log_retention_days
    );

    // proxy_manager 不需要直接访问 app_state，通过参数传递即可

    // 🆕 创建 API Key 配置的共享引用（用于热更新）
    // 使用 ArcSwap 实现无锁读取，提升并发性能
    let api_key_config = Arc::new(ArcSwap::from_pointee(config.api_key_auth.clone()));

    // 启动代理服务（如果启用）
    let (proxy_handle, pingora_service_opt, _pingora_shutdown_tx) = if let Some(proxy_config) =
        &config.proxy_config
    {
        info!(
            "Starting Pingora reverse proxy service, listening on port: {}",
            proxy_config.listen_port
        );
        info!(
            "Proxy route format: /proxy/{{port}}{{/path}} - e.g.: /proxy/{}/health",
            config.port
        );

        // 添加调试日志
        info!("🔧 [Pingora] startinginitialize Pingora config...");
        info!("🔧 [Pingora] listenport: {}", proxy_config.listen_port);
        info!(
            "🔧 [Pingora] Default backend port: {}",
            proxy_config.default_backend_port
        );
        info!("🔧 [Pingora] backend host: {}", proxy_config.backend_host);

        let pingora_config = ProxyConfig {
            listen_port: proxy_config.listen_port,
            default_backend_port: proxy_config.default_backend_port,
            backend_host: proxy_config.backend_host.clone(),
            port_param: proxy_config.port_param.clone(),
            config_file: None,
            verbose: false,
        };

        info!("[Pingora] Pingora configcreatedsucceeded");

        // 创建 Pingora 服务器管理器，并提取服务引用用于指标读取
        info!("🔧 [Pingora] created PingoraServerManager...");
        let mut server_manager = PingoraServerManager::new(pingora_config)
            .with_api_key_config(Arc::clone(&api_key_config)); // 🆕 传递 API Key 配置
        let pingora_service = server_manager.service();
        info!("[Pingora] PingoraServerManager createdsucceeded");
        info!("[Pingora] API Key config already loaded (no updates)");

        // 启动健康检查循环（按配置）
        if proxy_config.health_check.enabled {
            let hc = &proxy_config.health_check;
            info!(
                "🔧 [Pingora] Starting health check loop: interval={}s, timeout={}s",
                hc.interval_seconds, hc.timeout_seconds
            );
            pingora_service
                .start_health_check_loop(hc.interval_seconds, (hc.timeout_seconds * 1000) as u64);
            info!("[Pingora] health check already started");
        }

        // 启动 Pingora 服务器（如果启动失败，直接退出程序）
        info!("[Pingora] starting Pingora server...");
        let (pingora_shutdown_tx, pingora_shutdown_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            info!("📍 [Pingora] calling server_manager.start()...");
            if let Err(e) = server_manager.start(pingora_shutdown_rx).await {
                error!(
                    "[Pingora] Pingora proxy start failed, error: {:?}",
                    e
                );
                std::process::exit(1);
            }
            info!("[Pingora] server started");
        });

        info!("[Pingora] already started");

        (
            Some(handle),
            Some(pingora_service),
            Some(pingora_shutdown_tx),
        )
    } else {
        info!("⚠️ [Pingora] proxy_config notconfig, skip Pingora started");
        (None, None, None)
    };

    // 设置 Ctrl+C 信号处理
    let shutdown_tx = setup_signal_handlers();
    let shutdown_rx = shutdown_tx.subscribe();

    // 🆕 启动配置文件监控（支持 API Key 热更新）
    // 保持 watcher 的所有权，防止被提前 drop
    let config_path = std::path::PathBuf::from(crate::config::CONFIG_FILE);
    let _config_watcher =
        match crate::config_watcher::ConfigWatcher::new(config_path, Arc::clone(&api_key_config)) {
            Ok(watcher) => {
                info!("Config file watcher already started, API Key updated");
                Some(watcher)
            }
            Err(e) => {
                warn!(
                    "config file watcher start failed: {}, API Key updated",
                    e
                );
                None
            }
        };

    // 获取容器前缀（从配置读取，用于 pod_count 和 pod_list）
    let docker_config = config.docker_config.as_ref()
        .ok_or_else(|| anyhow::anyhow!("Docker config is required for container prefix"))?;
    let multi_config = docker_config.get_multi_image_config();
    let selector = docker_manager::image_selector::ImageSelector::new(multi_config);
    // 使用 block_in_place 在同步上下文中获取异步配置
    let (container_prefix_rcoder, container_prefix_computer) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let rcoder_prefix = selector
                .get_service_config(&shared_types::ServiceType::RCoder)
                .await
                .expect("Failed to get RCoder service config")
                .container_prefix()
                .to_string();
            let computer_prefix = selector
                .get_service_config(&shared_types::ServiceType::ComputerAgentRunner)
                .await
                .expect("Failed to get ComputerAgentRunner service config")
                .container_prefix()
                .to_string();
            (rcoder_prefix, computer_prefix)
        })
    });

    let state = Arc::new(AppState::new(
        config.clone(),
        pingora_service_opt,
        api_key_config,
        container_prefix_rcoder,
        container_prefix_computer,
    )?);

    // 在主异步运行时中启动清理任务（如果启用）
    let _cleanup_handle = if config.cleanup_config.enabled {
        let cleanup_config_clone = cleanup_config.clone();
        let state_for_cleanup = state.clone();
        Some(
            cleanup_task::start_cleanup_task(cleanup_config_clone, state_for_cleanup)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to start cleanup task: {}", e))?,
        )
    } else {
        info!("Container cleanup task already started (cleanup_config.enabled=false)");
        None
    };

    // 启动容器状态检查任务（防止长时间任务的容器被误杀）
    // 🆕 使用增强的配置，包含失败计数器和智能跳过机制
    let status_checker_config = ContainerStatusCheckerConfig {
        check_interval: Duration::from_secs(30), // 每 30 秒检查一次
        query_timeout: Duration::from_secs(5),   // 查询超时 5 秒
        failure_threshold: 3,                    // 连续失败 3 次后跳过
        skip_duration: Duration::from_secs(5 * 60), // 跳过 5 分钟
        health_reset_interval: Duration::from_secs(30 * 60), // 30 分钟清理一次
    };
    let _status_checker_handle =
        start_container_status_checker(status_checker_config, state.clone());
    info!(
        "Container status checker already started (interval: 30s, will skip Docker on failure)"
    );

    // 启动容器状态同步任务（定期检测被外部删除的容器）
    let container_sync_config = ContainerSyncConfig {
        sync_interval: Duration::from_secs(60), // 每 60 秒同步一次
    };
    let _container_sync_handle = start_container_sync_task(container_sync_config);
    info!(
        "Container status sync already started (interval: 60s, detect container)"
    );

    // 🆕 启动 VNC 后端同步任务（定期从 Docker 同步容器 IP 到 Pingora）
    if let Some(ref pingora_service) = state.pingora_service {
        let vnc_sync_config = VncSyncConfig {
            sync_interval: Duration::from_secs(5), // 每 5 秒同步一次
        };
        let _vnc_sync_handle = start_vnc_sync_task(pingora_service.clone(), vnc_sync_config);
        info!(
            "VNC sync already started (interval: 5s, sync Docker container IP)"
        );
    }

    // 创建路由（传入遥测 guard 用于 /metrics 端点）
    let app = router::create_router(state.clone(), Some(telemetry.clone()));

    // 启动 HTTP 服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .map_err(|e| anyhow::anyhow!("HTTP server failed to bind port {}: {}", config.port, e))?;

    info!("Server starting on port {}", config.port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent (legacy)");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks (unified stream)");
    info!("  GET  /health - Health check");
    info!("  NOTE: Plan data is delivered via the unified /progress/{{session_id}} SSE stream");

    if let Some(proxy_config) = &config.proxy_config {
        info!("Pingora proxy already started");
        info!("📡 listenport: {}", proxy_config.listen_port);
        info!("route: /proxy/{{port}}{{/path}} - example: /proxy/3000/api/users");
        info!("format: query port parameter to proxy request");
        info!("💡 example:");
        info!(
            "   http://localhost:{}/proxy/{}/health → http://127.0.0.1:{}/health",
            proxy_config.listen_port, config.port, config.port
        );
        info!(
            "   http://localhost:{}/proxy/{}/health → http://127.0.0.1:{}/health",
            proxy_config.listen_port, config.port, config.port
        );
        info!(
            "   http://localhost:{}/proxy/9000/health → http://127.0.0.1:9000/health (dynamic discovery)",
            proxy_config.listen_port
        );
    }

    // 启动服务器，支持优雅关闭
    // 使用自定义 Hyper 配置增加请求头大小限制（默认 8KB -> 128KB）
    info!("🔧 config HTTP max_buf_size = 128KB (to prevent HTTP 431 error)");

    let app = app.into_make_service();
    let mut shutdown_rx_clone = shutdown_tx.subscribe();

    // 启动自定义 HTTP 服务器
    let server_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                           // 等待关闭信号
                           _ = shutdown_rx_clone.recv() => {
            info!("🛑 HTTP server closed");
                               break;
                           }
                           // 接受新连接
                           result = listener.accept() => {
                               match result {
                                   Ok((stream, addr)) => {
                                       let mut app_clone = app.clone();

                                       tokio::spawn(async move {
                                           // 配置 HTTP1，增加 header 大小限制
                                           let mut http_builder = http1::Builder::new();
                                           http_builder
                                               .max_buf_size(128 * 1024)  // 128KB buffer（默认约 8KB）
                                               .preserve_header_case(true)
                                               .title_case_headers(false);

                                           let io = TokioIo::new(stream);

                                           // 使用 tower::Service 调用 MakeService
                                           use tower::Service;
                                           match std::future::poll_fn(|cx| {
                                               Service::<std::net::SocketAddr>::poll_ready(&mut app_clone, cx)
                                           }).await {
                                               Ok(()) => {
                                                   match Service::<std::net::SocketAddr>::call(&mut app_clone, addr).await {
                                                       Ok(service) => {
                                                           let hyper_service = TowerToHyperService::new(service);
                                                           if let Err(e) = http_builder.serve_connection(io, hyper_service).await {
                                                               if !e.to_string().contains("connection closed")
                                                                  && !e.to_string().contains("early eof") {
            tracing::debug!("HTTP connectionerror ({}): {}", addr, e);
                                                               }
                                                           }
                                                       }
                                                       Err(_) => {
                                                           // Infallible 类型，不会发生
                                                       }
                                                   }
                                               }
                                               Err(e) => {
            tracing::error!("server error: {}", e);
                                               }
                                           }
                                       });
                                   }
                                   Err(e) => {
            error!("connection failed: {}", e);
                                   }
                               }
                           }
                       }
        }
    });

    // 等待服务器关闭
    let _ = shutdown_signal(shutdown_rx).await;
    server_handle.abort();

    // 等待代理服务完成
    if let Some(handle) = proxy_handle {
        handle.await?;
    }

    Ok(())
}

/// 显示 Docker 配置帮助信息
fn show_docker_configuration_help(socket_path: &str) {
    error!("📋 Docker config help:");
    error!("");
    error!("add to docker-compose.yml config:");
    error!("");
    error!("services:");
    error!("  rcoder:");
    error!("    environment:");
    error!("      - DOCKER_SOCKET_PATH={}", socket_path);
    error!("    volumes:");
    error!("      - {}:/var/run/docker.sock:ro", socket_path);
    error!("      - ./data/rcoder/project_workspace:/app/project_workspace");
    error!("");
    error!("🔧 Docker socket path:");
    error!(" Linux: /var/run/docker.sock");
    error!("  macOS + Docker Desktop: /var/run/docker.sock");
    error!("  Rootless Docker: /run/user/$UID/docker.sock");
    error!("");
    error!("🛠️ troubleshooting:");
    error!("1. check Docker: docker ps");
    error!("2. check socket file exists: ls -l {}", socket_path);
    error!("3. check docker group: groups $USER | grep docker");
    error!(
        "  4. Test Docker API: curl --unix-socket {} http://localhost/info",
        socket_path
    );
    error!("");
    error!("socket exists, rcoder container may not have access");
}

/// 设置信号处理器
fn setup_signal_handlers() -> tokio::sync::broadcast::Sender<()> {
    let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);

    // 设置全局关闭标志
    static SHUTDOWN_INITIATED: AtomicBool = AtomicBool::new(false);

    // 注册 Ctrl+C 信号处理
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let shutdown_tx_clone = shutdown_tx.clone();
        tokio::spawn(async move {
            // 注册信号处理器，如果失败则记录警告并优雅降级
            let sigint_result = signal(SignalKind::interrupt());
            let sigterm_result = signal(SignalKind::terminate());

            match (sigint_result, sigterm_result) {
                (Ok(mut sigint), Ok(mut sigterm)) => {
                    tokio::select! {
                                           _ = sigint.recv() => {
                                               if !SHUTDOWN_INITIATED.swap(true, Ordering::SeqCst) {
                    info!(" received SIGINT (Ctrl+C), starting graceful shutdown...");
                                                   let _ = shutdown_tx_clone.send(());
                                               }
                                           }
                                           _ = sigterm.recv() => {
                                               if !SHUTDOWN_INITIATED.swap(true, Ordering::SeqCst) {
                    info!(" received SIGTERM, starting graceful shutdown...");
                                                   let _ = shutdown_tx_clone.send(());
                                               }
                                           }
                                       }
                }
                (Err(e), _) | (_, Err(e)) => {
                    warn!(" unix signal handler failed: {}, shutdown may not be graceful", e);
                    // 注册失败不影响程序运行，仍可通过其他方式关闭（如 tokio::signal::ctrl_c）
                }
            }
        });
    }

    #[cfg(not(unix))]
    {
        let shutdown_tx_clone = shutdown_tx.clone();
        tokio::spawn(async move {
            use tokio::signal;

            if let Ok(()) = signal::ctrl_c().await {
                if !SHUTDOWN_INITIATED.swap(true, Ordering::SeqCst) {
                    info!(" received Ctrl+C, starting graceful shutdown...");
                    let _ = shutdown_tx_clone.send(());
                }
            }
        });
    }

    shutdown_tx
}

/// 优雅关闭信号处理
async fn shutdown_signal(mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) {
    // 等待关闭信号
    let _ = shutdown_rx.recv().await;

    info!("starting graceful shutdown...");

    // 执行容器清理
    if let Err(e) = cleanup_all_containers().await {
        error!("container cleanup failed: {}", e);
    } else {
        info!("container cleanup completed");
    }

    info!("🛑 RCoder graceful shutdown completed");
}

/// 清理所有动态创建的容器
async fn cleanup_all_containers() -> anyhow::Result<()> {
    info!("🧹 starting cleanup of dynamically created containers...");

    match docker_manager::runtime::RuntimeManager::runtime_type() {
        RuntimeType::Docker => {
            let docker_manager = docker_manager::global::get_global_docker_manager()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get global DockerManager: {}", e))?;

            let multi_image_config = shared_types::create_default_multi_image_config();
            match container_stop::startup_cleanup_all_enabled_services(
                &docker_manager,
                &multi_image_config,
            )
            .await
            {
                Ok(result) => {
                    if result.successfully_removed > 0 {
                        info!(
                            "🧹 Cleaned up {} containers (all enabled services)",
                            result.successfully_removed
                        );
                    }

                    if result.failed_removals > 0 {
                        warn!(
                            "container cleanup failed: failed count={}",
                            result.failed_removals
                        );
                    }
                }
                Err(e) => {
                    warn!("container cleanup error: {}", e);
                }
            }
        }
        RuntimeType::Kubernetes => {
            let runtime = docker_manager::runtime::RuntimeManager::get()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get runtime: {}", e))?;
            runtime
                .cleanup_all()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to cleanup runtime resources: {}", e))?;
        }
    }

    Ok(())
}
