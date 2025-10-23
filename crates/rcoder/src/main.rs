use clap::Parser;
use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
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

    // 创建清理配置
    let cleanup_config = CleanupConfig {
        idle_timeout: Duration::from_secs(3600),
        cleanup_interval: Duration::from_secs(30),
    };

    // 在主异步运行时中启动清理任务
    let _cleanup_handle = start_cleanup_task(cleanup_config.clone());

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

    // 初始化全局 DockerManager
    info!("🐳 初始化全局 DockerManager...");
    // 使用 rcoder 配置创建 DockerManager 配置
    let docker_config = docker_manager::utils::DockerUtils::config_from_rcoder_docker_config(
        config.docker_config.as_ref(),
    );
    if let Err(e) =
        docker_manager::global::init_global_docker_manager_with_config(docker_config).await
    {
        error!("❌ 全局 DockerManager 初始化失败: {}", e);
        return Err(anyhow::anyhow!("全局 DockerManager 初始化失败: {}", e));
    }
    info!("✅ 全局 DockerManager 初始化成功");

    let state = Arc::new(AppState {
        sessions: Arc::new(DashMap::new()),
        config: config.clone(),
        pingora_service: pingora_service_opt,
    });

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

    axum::serve(listener, app).await?;

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
