use clap::Parser;
use dashmap::DashMap;
use shared_types::{ProjectAndAgentInfo, ProjectAndContainerInfo};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tracing::{debug, error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod handler;
mod proxy_agent;

mod middleware;
mod router;
mod service;
mod utils;

use rcoder::*;

use config::{CliArgs, load_config_with_args};
use pingora_proxy::{PingoraProxyService, PingoraServerManager, ProxyConfig};
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

    // 🔄 初始化宿主机路径解析器（自动检测模式）
    info!("🔍 开始自动检测宿主机挂载路径...");
    let docker_socket_path = std::env::var("DOCKER_SOCKET_PATH").unwrap_or_else(|_| {
        info!("环境变量 DOCKER_SOCKET_PATH 未设置，使用默认值: /var/run/docker.sock");
        "/var/run/docker.sock".to_string()
    });

    info!("使用 Docker socket: {}", docker_socket_path);

    let path_resolver =
        match utils::HostPathResolver::new_with_docker_socket(&docker_socket_path).await {
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
    // 先初始化全局 DockerManager
    info!("🔍 初始化 Docker Manager...");
    let docker_manager = match docker_manager::global::get_global_docker_manager().await {
        Ok(dm) => {
            info!("✅ Docker Manager 初始化成功");
            dm
        }
        Err(e) => {
            error!("❌ Docker Manager 初始化失败: {}", e);
            return Err(anyhow::anyhow!("Docker Manager 初始化失败: {}", e));
        }
    };

    info!("🔍 检查并清理上次可能遗留的 rcoder-agent 容器...");
    match startup_cleanup_orphaned_containers(&docker_manager).await {
        Ok(cleaned_count) => {
            if cleaned_count > 0 {
                info!("✅ 启动时清理完成，共清理了 {} 个遗留容器", cleaned_count);
            } else {
                info!("✅ 未发现遗留容器，系统环境干净");
            }
        }
        Err(e) => {
            warn!("⚠️ 启动时容器清理失败: {}，但这不影响服务启动", e);
        }
    }

    // 创建清理配置
    let cleanup_config = CleanupConfig {
        idle_timeout: Duration::from_secs(3600),
        cleanup_interval: Duration::from_secs(30),
    };

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

    // 设置 Ctrl+C 信号处理
    let shutdown_tx = setup_signal_handlers();
    let mut shutdown_rx = shutdown_tx.subscribe();

    let state = Arc::new(AppState::new(config.clone(), pingora_service_opt));

    // 在主异步运行时中启动清理任务
    let _cleanup_handle = start_cleanup_task(cleanup_config.clone(), state.clone());

    // 创建路由
    let app = router::create_router(state.clone());

    // 启动 HTTP 服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .unwrap();

    info!("Server starting on port {}", config.port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent (legacy)");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks (unified stream)");
    info!("  GET  /health - Health check");
    info!("  NOTE: Plan data is delivered via the unified /progress/{{session_id}} SSE stream");

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

    // 启动服务器，支持优雅关闭
    let server_handle = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await;

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

    // 设置按天滚动的文件 appender
    let file_appender = RollingFileAppender::new(Rotation::DAILY, logs_dir, "rcoder");

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
            let mut sigint = signal(SignalKind::interrupt()).unwrap();
            let mut sigterm = signal(SignalKind::terminate()).unwrap();

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

    let mut cleaned_count = 0;
    let mut failed_count = 0;

    // 直接查找并清理所有 rcoder-agent-* 容器（不再依赖全局映射）
    info!("开始查找所有 rcoder-agent 容器...");

    // 额外检查：查找可能遗漏的 rcoder-agent-* 容器
    match find_and_cleanup_orphaned_containers(&docker_manager).await {
        Ok(orphaned_cleaned) => {
            if orphaned_cleaned > 0 {
                info!("🧹 清理了 {} 个孤立的 rcoder-agent 容器", orphaned_cleaned);
                cleaned_count += orphaned_cleaned;
            }
        }
        Err(e) => {
            warn!("查找孤立容器时出错: {}", e);
        }
    }

    info!(
        "🧹 容器清理完成: 成功={}, 失败={}",
        cleaned_count, failed_count
    );

    Ok(())
}

/// 查找并清理孤立的 rcoder-agent 容器
async fn find_and_cleanup_orphaned_containers(
    docker_manager: &docker_manager::DockerManager,
) -> anyhow::Result<u64> {
    info!("🔍 查找孤立的 rcoder-agent 容器...");

    // 使用新的模式匹配清理功能
    let cleanup_options = docker_manager::CleanupOptions {
        force_remove_running: true, // 强制删除运行中的容器（服务关闭时）
        wait_for_graceful_stop: true,
        stop_timeout_seconds: 30,
        remove_associated_volumes: false,
    };

    // 清理所有 rcoder-agent-* 容器
    match docker_manager
        .cleanup_containers_with_pattern("rcoder-agent-*", cleanup_options)
        .await
    {
        Ok(result) => {
            info!(
                "🧹 孤立容器清理完成: 找到={}, 成功={}, 失败={}, 成功率={:.1}%",
                result.total_found,
                result.successfully_removed,
                result.failed_removals,
                result.success_rate()
            );

            if result.has_removals() {
                info!("📋 被删除的容器IDs: {:?}", result.removed_container_ids);
            }

            if !result.is_complete_success() {
                warn!("⚠️ 部分容器清理失败:");
                for failure in &result.failed_removals_details {
                    warn!(
                        "  - 容器 {} ({}): {}",
                        failure.container_id, failure.container_name, failure.error_message
                    );
                }
            }

            Ok(result.successfully_removed as u64)
        }
        Err(e) => {
            error!("❌ 孤立容器清理失败: {}", e);
            Err(anyhow::anyhow!("清理孤立容器失败: {}", e))
        }
    }
}

/// 启动时清理遗留的 rcoder-agent 容器
/// 使用更激进的清理策略，确保启动时环境干净
async fn startup_cleanup_orphaned_containers(
    docker_manager: &docker_manager::DockerManager,
) -> anyhow::Result<u64> {
    info!("🧹 启动时清理：查找并清理所有 rcoder-agent-* 容器...");

    // 启动时使用更激进的清理策略
    let cleanup_options = docker_manager::CleanupOptions {
        force_remove_running: true,       // 强制删除运行中的容器
        wait_for_graceful_stop: false,    // 不等待优雅停止，直接删除
        stop_timeout_seconds: 5,          // 很短的超时时间
        remove_associated_volumes: false, // 不删除数据卷
    };

    // 清理所有 rcoder-agent-* 容器
    match docker_manager
        .cleanup_containers_with_pattern("rcoder-agent-*", cleanup_options)
        .await
    {
        Ok(result) => {
            if result.total_found > 0 {
                info!(
                    "🧹 启动清理完成: 找到={}, 成功={}, 失败={}",
                    result.total_found, result.successfully_removed, result.failed_removals
                );

                if result.has_removals() {
                    info!("📋 被清理的容器IDs: {:?}", result.removed_container_ids);
                }

                if !result.is_complete_success() {
                    // 过滤掉 409 冲突错误（容器已在删除中），这是正常情况
                    let mut real_failures = Vec::new();
                    for failure in &result.failed_removals_details {
                        if failure.error_message.contains("409")
                            && failure.error_message.contains("already in progress")
                        {
                            info!(
                                "  - 容器 {} ({}): 已在删除中，跳过",
                                failure.container_id, failure.container_name
                            );
                        } else {
                            real_failures.push(failure);
                        }
                    }

                    if !real_failures.is_empty() {
                        warn!("⚠️ 部分容器清理失败:");
                        for failure in real_failures {
                            warn!(
                                "  - 容器 {} ({}): {}",
                                failure.container_id, failure.container_name, failure.error_message
                            );
                        }
                    }
                }
            } else {
                debug!("🧹 启动清理: 未发现 rcoder-agent-* 容器");
            }

            Ok(result.successfully_removed as u64)
        }
        Err(e) => {
            // 检查是否是 409 冲突错误（容器已在删除中）
            let error_str = e.to_string();
            if error_str.contains("409") && error_str.contains("already in progress") {
                info!("🔄 启动时遇到容器正在删除中的情况，这是正常的");
                Ok(0)
            } else {
                error!("❌ 启动时容器清理失败: {}", e);
                Err(anyhow::anyhow!("启动时清理失败: {}", e))
            }
        }
    }
}
