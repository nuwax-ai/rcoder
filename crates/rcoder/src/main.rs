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

// 路由创建函数已移动到 handler 模块

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 🆕 初始化遥测系统（使用 rcoder-telemetry，包含控制台 + 文件日志）
    let telemetry_config = TelemetryConfig::from_env("rcoder").with_file_log("rcoder"); // 启用文件日志，前缀为 rcoder
    let telemetry: TelemetryGuard = rcoder_telemetry::init(telemetry_config).await?;
    let telemetry = Arc::new(telemetry);

    info!("Starting rcoder - AI-powered development platform");

    // 解析命令行参数
    let cli_args = CliArgs::parse();

    // 加载配置（包含命令行参数）
    let config = load_config_with_args(cli_args)?;

    // 创建项目工作目录
    tokio::fs::create_dir_all(&config.projects_dir).await?;
    info!("Projects directory: {:?}", config.projects_dir);

    // 🔄 初始化宿主机路径解析器（自动检测模式）
    info!("🔍 开始自动检测宿主机挂载路径...");
    let docker_socket_path = std::env::var("DOCKER_SOCKET_PATH").unwrap_or_else(|_| {
        info!("环境变量 DOCKER_SOCKET_PATH 未设置，使用默认值: /var/run/docker.sock");
        "/var/run/docker.sock".to_string()
    });

    info!("使用 Docker socket: {}", docker_socket_path);

    let _path_resolver =
        match utils::HostPathResolver::new_with_docker_socket(Some(docker_socket_path.clone()))
            .await
        {
            Ok(resolver) => {
                info!("✅ 宿主机路径解析器初始化成功");
                info!(
                    "  容器内工作目录: {:?}",
                    resolver.container_workspace_base()
                );
                info!("  宿主机工作目录: {:?}", resolver.host_workspace_base());
                Some(resolver)
            }
            Err(e) => {
                error!("❌ 宿主机路径解析器初始化失败: {}", e);
                error!("请检查以下配置:");
                error!("  1. Docker socket 路径是否正确: {}", docker_socket_path);
                error!("  2. Docker socket 是否已挂载到容器");
                error!("  3. 容器是否有权限访问 Docker API");
                error!("  4. 项目工作目录是否正确挂载");

                // 显示详细的错误信息和解决建议
                show_docker_configuration_help(&docker_socket_path);

                // 返回错误，停止启动
                return Err(anyhow::anyhow!("容器自检测失败，无法初始化路径解析器"));
            }
        };

    // 🧹 启动时清理上次可能遗留的容器
    // 先使用正确配置初始化全局 DockerManager
    info!("🔍 初始化 Docker Manager（使用应用配置）...");

    // 从应用配置创建 DockerManagerConfig
    let docker_manager_config = if let Some(docker_config) = &config.docker_config {
        info!("✅ 使用应用中的 Docker 配置，合并多镜像配置");
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
            info!("✅ 使用配置中的网络基础名称: {}", network_base_name);
            default_config.network_base_name = network_base_name.clone();
        } else {
            info!(
                "⚠️ 配置中无 network_base_name，使用默认值: {}",
                default_config.network_base_name
            );
        }

        default_config
    } else {
        info!("⚠️  应用中无 Docker 配置，使用默认配置");
        docker_manager::DockerManagerConfig::default()
    };

    // 使用自定义配置初始化全局 DockerManager
    if let Err(e) =
        docker_manager::global::init_global_docker_manager_with_config(docker_manager_config).await
    {
        error!("❌ Docker Manager 初始化失败: {}", e);
        return Err(anyhow::anyhow!("Docker Manager 初始化失败: {}", e));
    }

    // 获取初始化后的 DockerManager
    let docker_manager = match docker_manager::global::get_global_docker_manager().await {
        Ok(dm) => {
            info!("✅ Docker Manager 初始化成功（使用应用配置）");
            dm
        }
        Err(e) => {
            error!("❌ 获取 Docker Manager 失败: {}", e);
            return Err(anyhow::anyhow!("获取 Docker Manager 失败: {}", e));
        }
    };

    // 🔧 从配置获取多镜像配置用于容器清理
    let multi_image_config = if let Some(docker_config) = &config.docker_config {
        docker_config.get_multi_image_config()
    } else {
        shared_types::create_default_multi_image_config()
    };

    info!("🔍 检查并清理上次可能遗留的容器（所有启用的服务）...");
    match container_stop::startup_cleanup_all_enabled_services(&docker_manager, &multi_image_config)
        .await
    {
        Ok(result) => {
            let enabled_services = shared_types::get_enabled_service_types(&multi_image_config);
            if result.successfully_removed > 0 {
                info!(
                    "✅ 启动时清理完成，共清理了 {} 个遗留容器（涵盖 {} 个服务类型）",
                    result.successfully_removed,
                    enabled_services.len()
                );
            } else {
                info!("✅ 未发现遗留容器，系统环境干净");
            }

            // 如果有失败的清理（非409错误），记录警告
            if result.failed_removals > 0 {
                warn!("⚠️ 部分容器清理失败: 失败数量={}", result.failed_removals);
                for failure in &result.failed_removals_details {
                    warn!(
                        "  - 容器 {} ({}): {}",
                        failure.container_id, failure.container_name, failure.error_message
                    );
                }
            }
        }
        Err(e) => {
            warn!("⚠️ 启动时容器清理失败: {}，但这不影响服务启动", e);
        }
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
    };
    info!(
        "🧹 清理配置: 闲置超时={}秒, 清理间隔={}秒, Docker停止超时={}秒, 容器保护时间={}秒",
        config.cleanup_config.idle_timeout_seconds,
        config.cleanup_config.cleanup_interval_seconds,
        config.cleanup_config.docker_stop_timeout_seconds,
        config.cleanup_config.container_protection_seconds
    );

    // proxy_manager 不需要直接访问 app_state，通过参数传递即可

    // 🆕 创建 API Key 配置的共享引用（用于热更新）
    // 使用 ArcSwap 实现无锁读取，提升并发性能
    let api_key_config = Arc::new(ArcSwap::from_pointee(config.api_key_auth.clone()));

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

        // 添加调试日志
        info!("🔧 [Pingora] 开始初始化 Pingora 配置...");
        info!("🔧 [Pingora] 监听端口: {}", proxy_config.listen_port);
        info!(
            "🔧 [Pingora] 默认后端端口: {}",
            proxy_config.default_backend_port
        );
        info!("🔧 [Pingora] 后端主机: {}", proxy_config.backend_host);

        let pingora_config = ProxyConfig {
            listen_port: proxy_config.listen_port,
            default_backend_port: proxy_config.default_backend_port,
            backend_host: proxy_config.backend_host.clone(),
            port_param: proxy_config.port_param.clone(),
            config_file: None,
            verbose: false,
        };

        info!("✅ [Pingora] Pingora 配置创建成功");

        // 创建 Pingora 服务器管理器，并提取服务引用用于指标读取
        info!("🔧 [Pingora] 创建 PingoraServerManager...");
        let mut server_manager = PingoraServerManager::new(pingora_config)
            .with_api_key_config(Arc::clone(&api_key_config)); // 🆕 传递 API Key 配置
        let pingora_service = server_manager.service();
        info!("✅ [Pingora] PingoraServerManager 创建成功");
        info!("🔒 [Pingora] API Key 鉴权配置已注入（支持热更新）");

        // 启动健康检查循环（按配置）
        if proxy_config.health_check.enabled {
            let hc = &proxy_config.health_check;
            info!(
                "🔧 [Pingora] 启动健康检查循环: interval={}s, timeout={}s",
                hc.interval_seconds, hc.timeout_seconds
            );
            pingora_service
                .start_health_check_loop(hc.interval_seconds, (hc.timeout_seconds * 1000) as u64);
            info!("✅ [Pingora] 健康检查循环已启动");
        }

        // 启动 Pingora 服务器（如果启动失败，直接退出程序）
        info!("🚀 [Pingora] 启动 Pingora 服务器...");
        let handle = tokio::spawn(async move {
            info!("📍 [Pingora] 正在调用 server_manager.start()...");
            if let Err(e) = server_manager.start().await {
                error!("❌ [Pingora] Pingora 代理服务器启动失败，程序退出: {:?}", e);
                std::process::exit(1);
            }
            info!("✅ [Pingora] Pingora 服务器正常退出");
        });

        info!("✅ [Pingora] 后台任务已启动");

        (Some(handle), Some(pingora_service))
    } else {
        info!("⚠️  [Pingora] proxy_config 未配置，跳过 Pingora 启动");
        (None, None)
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
                info!("🔄 配置文件监控已启动，支持 API Key 热更新");
                Some(watcher)
            }
            Err(e) => {
                warn!("⚠️  配置文件监控启动失败: {}，API Key 热更新将不可用", e);
                None
            }
        };

    let state = Arc::new(AppState::new(
        config.clone(),
        pingora_service_opt,
        api_key_config,
    )?);

    // 在主异步运行时中启动清理任务
    let cleanup_config_clone = cleanup_config.clone();
    let state_for_cleanup = state.clone();
    let _cleanup_handle = cleanup_task::start_cleanup_task(cleanup_config_clone, state_for_cleanup)
        .await
        .map_err(|e| anyhow::anyhow!("清理任务启动失败: {}", e))?;

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
    info!("🔍 容器状态检查任务已启动（间隔: 30 秒，启用 Docker 主动查询和失败计数器）");

    // 启动容器状态同步任务（定期检测被外部删除的容器）
    let container_sync_config = ContainerSyncConfig {
        sync_interval: Duration::from_secs(60), // 每 60 秒同步一次
    };
    let _container_sync_handle = start_container_sync_task(container_sync_config);
    info!("🔄 容器状态同步任务已启动（间隔: 60 秒，检测外部删除的容器）");

    // 🆕 启动 VNC 后端同步任务（定期从 Docker 同步容器 IP 到 Pingora）
    if let Some(ref pingora_service) = state.pingora_service {
        let vnc_sync_config = VncSyncConfig {
            sync_interval: Duration::from_secs(5), // 每 5 秒同步一次
        };
        let _vnc_sync_handle = start_vnc_sync_task(pingora_service.clone(), vnc_sync_config);
        info!("🔗 VNC 后端同步任务已启动（间隔: 5 秒，从 Docker 同步容器 IP）");
    }

    // 创建路由（传入遥测 guard 用于 /metrics 端点）
    let app = router::create_router(state.clone(), Some(telemetry.clone()));

    // 启动 HTTP 服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .map_err(|e| anyhow::anyhow!("HTTP 服务器绑定端口 {} 失败: {}", config.port, e))?;

    info!("Server starting on port {}", config.port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent (legacy)");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks (unified stream)");
    info!("  GET  /health - Health check");
    info!("  NOTE: Plan data is delivered via the unified /progress/{{session_id}} SSE stream");

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

    // 启动服务器，支持优雅关闭
    // 使用自定义 Hyper 配置增加请求头大小限制（默认 8KB -> 128KB）
    info!("🔧 配置 HTTP 服务器：max_buf_size = 128KB（解决 HTTP 431 错误）");

    let app = app.into_make_service();
    let mut shutdown_rx_clone = shutdown_tx.subscribe();

    // 启动自定义 HTTP 服务器
    let server_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                // 等待关闭信号
                _ = shutdown_rx_clone.recv() => {
                    info!("🛑 HTTP 服务器收到关闭信号");
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
                                                        tracing::debug!("HTTP 连接错误 ({}): {}", addr, e);
                                                    }
                                                }
                                            }
                                            Err(_) => {
                                                // Infallible 类型，不会发生
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("服务未就绪: {}", e);
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            error!("接受连接失败: {}", e);
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
    error!("📋 Docker 配置帮助:");
    error!("");
    error!("请确保您的 docker-compose.yml 包含以下配置:");
    error!("");
    error!("services:");
    error!("  rcoder:");
    error!("    environment:");
    error!("      - DOCKER_SOCKET_PATH={}", socket_path);
    error!("    volumes:");
    error!("      - {}:/var/run/docker.sock:ro", socket_path);
    error!("      - ./data/rcoder/project_workspace:/app/project_workspace");
    error!("");
    error!("🔧 常见 Docker socket 路径:");
    error!("  Linux 系统: /var/run/docker.sock");
    error!("  macOS + Docker Desktop: /var/run/docker.sock");
    error!("  Rootless Docker: /run/user/$UID/docker.sock");
    error!("");
    error!("🛠️ 故障排除步骤:");
    error!("  1. 检查 Docker 是否正在运行: docker ps");
    error!("  2. 验证 socket 文件存在: ls -l {}", socket_path);
    error!("  3. 检查权限: groups $USER | grep docker");
    error!(
        "  4. 测试 Docker API: curl --unix-socket {} http://localhost/info",
        socket_path
    );
    error!("");
    error!("如果问题持续存在，请查看 rcoder 容器日志获取更多详细信息。");
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
                                info!("收到 SIGINT (Ctrl+C) 信号，开始优雅关闭...");
                                let _ = shutdown_tx_clone.send(());
                            }
                        }
                        _ = sigterm.recv() => {
                            if !SHUTDOWN_INITIATED.swap(true, Ordering::SeqCst) {
                                info!("收到 SIGTERM 信号，开始优雅关闭...");
                                let _ = shutdown_tx_clone.send(());
                            }
                        }
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    warn!("⚠️  Unix 信号处理器注册失败: {}，将依赖其他关闭机制", e);
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
                    info!("收到 Ctrl+C 信号，开始优雅关闭...");
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

    info!("🔄 开始优雅关闭流程...");

    // 执行容器清理
    if let Err(e) = cleanup_all_containers().await {
        error!("❌ 容器清理失败: {}", e);
    } else {
        info!("✅ 容器清理完成");
    }

    info!("🛑 RCoder 服务优雅关闭完成");
}

/// 清理所有动态创建的容器
async fn cleanup_all_containers() -> anyhow::Result<()> {
    info!("🧹 开始清理所有动态创建的容器...");

    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| anyhow::anyhow!("获取全局 DockerManager 失败: {}", e))?;

    // 🔧 使用默认多镜像配置
    // 注意：在关闭时使用默认配置是安全的，因为启用的服务类型在默认配置中已定义
    let multi_image_config = shared_types::create_default_multi_image_config();

    // 使用启动清理策略（服务关闭时也使用相同策略）
    match container_stop::startup_cleanup_all_enabled_services(&docker_manager, &multi_image_config)
        .await
    {
        Ok(result) => {
            if result.successfully_removed > 0 {
                info!(
                    "🧹 清理了 {} 个容器（所有启用的服务）",
                    result.successfully_removed
                );
            }

            if result.failed_removals > 0 {
                warn!("⚠️ 部分容器清理失败: 失败数量={}", result.failed_removals);
            }
        }
        Err(e) => {
            warn!("查找孤立容器时出错: {}", e);
        }
    }

    Ok(())
}
