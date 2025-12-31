use clap::Parser;
use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
use tracing_appender::rolling::Rotation;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod agent_worker_manager;
mod api_key_manager;
mod config;
mod grpc;
mod handler;
mod model;
mod proxy_agent;

mod middleware;
mod router;
mod service;
mod utils;

use agent_worker_manager::AgentWorkerManager;
use config::{CliArgs, load_config_with_args};
use model::*;
use proxy_agent::cleanup_task::{CleanupConfig, start_cleanup_task};
use rcoder_proxy::{PingoraServerManager, ProxyConfig};
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

    // 🔥 创建 AgentWorkerManager（返回 manager, heartbeat_rx, ready_rx, heartbeat_tx, ready_tx）
    let (worker_manager, heartbeat_rx, ready_rx, heartbeat_tx, ready_tx) =
        AgentWorkerManager::new();
    let worker_manager = Arc::new(worker_manager);

    // 创建 worker handle（包含心跳和就绪通道）
    let worker_handle = worker_manager.create_handle(heartbeat_tx, ready_tx);
    info!("🔧 [MAIN] AgentWorkerManager 已创建");

    // 创建初始通道
    let (local_task_sender, local_task_receiver) = tokio::sync::mpsc::unbounded_channel();

    // 🔥 设置初始 sender 到 worker_manager（传递引用）
    worker_manager.set_sender(&local_task_sender);

    // 🔥 在独立 OS 线程中启动单线程 tokio 运行时 + LocalSet，驻留运行 agent_worker（!Send）
    let _worker_thread = std::thread::spawn(move || {
        let _ = run_agent_worker_thread(local_task_receiver, worker_handle);
    });

    info!("📌 [MAIN] agent_worker 线程已启动");

    // 🔥 启动 worker 监控任务（使用 Arc 共享）
    let worker_manager_for_monitor = worker_manager.clone();
    tokio::spawn(async move {
        let _ = monitor_worker_health(worker_manager_for_monitor, heartbeat_rx, ready_rx).await;
    });
    info!("🔍 [MAIN] Worker 监控任务已启动");

    // 创建清理配置
    let cleanup_config = CleanupConfig {
        idle_timeout: Duration::from_secs(3600),
        cleanup_interval: Duration::from_secs(30),
    };

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

    // 🔥 创建 ApiKeyManager 包装器（包装共享 DashMap，消除双重存储）
    let api_key_manager = Arc::new(api_key_manager::ApiKeyManager::from_shared(
        shared_api_key_manager.clone(),
    ));

    // 🔒 project_id -> service_uuid 映射
    let project_uuid_map = Arc::new(DashMap::new());

    let state = Arc::new(AppState {
        sessions: Arc::new(DashMap::new()),
        config: config.clone(),
        local_task_sender,
        agent_worker_manager: worker_manager.clone(), // 🆕 添加 worker_manager
        pingora_service: pingora_service_opt,
        api_key_manager, // 现在包装 shared_api_key_manager
        shared_api_key_manager,
        project_uuid_map,
    });

    // 创建路由
    let app = router::create_router(state.clone());

    // 启动 gRPC 服务器
    let grpc_port = shared_types::GRPC_DEFAULT_PORT;
    let grpc_addr = format!("[::]:{}", grpc_port)
        .parse()
        .map_err(|e| anyhow::anyhow!("gRPC 地址解析失败: {}", e))?;
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

/// 🔥 新增：agent_worker 线程启动函数
///
/// 在独立的 OS 线程中运行单线程 tokio 运行时 + LocalSet
/// 因为 ACP 连接不是 Send，必须在 LocalSet 中运行
fn run_agent_worker_thread(
    receiver: tokio::sync::mpsc::UnboundedReceiver<proxy_agent::LocalSetAgentRequest>,
    handle: agent_worker_manager::WorkerHandle,
) -> anyhow::Result<()> {
    info!("🚀 [agent_worker_thread] 线程启动，开始创建 LocalSet 运行时...");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to build single-thread runtime for LocalSet agents");

    rt.block_on(async move {
        info!("🚀 [agent_worker_thread] 开始运行 LocalSet...");

        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async move {
                info!("🚀 [agent_worker_thread] LocalSet 已启动，开始监听任务...");

                // 🆕 使用带心跳的 agent_worker
                match proxy_agent::agent_worker_with_heartbeat(receiver, handle).await {
                    Ok(_) => {
                        info!("✅ [agent_worker_thread] Agent worker 正常退出");
                    }
                    Err(e) => {
                        error!("❌ [agent_worker_thread] Agent worker 失败: {}", e);
                    }
                }

                warn!("⚠️ [agent_worker_thread] Agent worker stopped");
            })
            .await;

        info!("🔚 [agent_worker_thread] LocalSet 已停止");
        Ok::<(), anyhow::Error>(())
    })
}

/// 🔥 新增：监控 worker 健康状态
///
/// 监听心跳信号，检测 worker 是否崩溃，并自动重启
///
/// 🆕 优化：增加 ready_rx 参数，等待 Worker 发送就绪信号后才设置 Running 状态
async fn monitor_worker_health(
    worker_manager: Arc<AgentWorkerManager>,
    mut heartbeat_rx: tokio::sync::mpsc::Receiver<agent_worker_manager::Heartbeat>,
    ready_rx: tokio::sync::oneshot::Receiver<agent_worker_manager::WorkerReady>,
) -> anyhow::Result<()> {
    use tokio::time::{Duration, interval, timeout};

    info!("🔍 [WorkerMonitor] 健康监控任务已启动，等待 Worker 就绪...");

    // 🆕 等待 Worker 发送就绪信号（带 30 秒超时，oneshot）
    let mut initial_ready_failed = false;
    match timeout(Duration::from_secs(30), ready_rx).await {
        Ok(Ok(_ready)) => {
            worker_manager.update_state(agent_worker_manager::WorkerState::Running);
            info!("✅ [WorkerMonitor] Worker 就绪，状态已更新为 Running");
        }
        Ok(Err(_)) => {
            error!("❌ [WorkerMonitor] Ready 通道意外关闭，将触发重启");
            initial_ready_failed = true;
        }
        Err(_) => {
            error!("❌ [WorkerMonitor] Worker 启动超时（30 秒未收到就绪信号），将触发重启");
            initial_ready_failed = true;
        }
    }

    // 🆕 如果初始 Ready 失败，立即触发重启
    if initial_ready_failed {
        let (new_hb_rx, new_ready_rx) = restart_worker(worker_manager.clone()).await?;
        heartbeat_rx = new_hb_rx;
        // 等待重启后的 Ready 信号
        match timeout(Duration::from_secs(30), new_ready_rx).await {
            Ok(Ok(_ready)) => {
                worker_manager.update_state(agent_worker_manager::WorkerState::Running);
                info!("✅ [WorkerMonitor] 重启后 Worker 就绪，状态已更新为 Running");
            }
            _ => {
                error!("❌ [WorkerMonitor] 重启后 Worker 仍未就绪，继续监控...");
            }
        }
    }

    let mut heartbeat_check = interval(Duration::from_secs(5));
    heartbeat_check.tick().await; // 跳过第一次立即触发

    loop {
        tokio::select! {
            // 接收心跳信号
            heartbeat = heartbeat_rx.recv() => {
                if let Some(hb) = heartbeat {
                    worker_manager.update_heartbeat(hb);
                } else {
                    // 心跳通道关闭，可能监控任务已关闭
                    warn!("⚠️ [WorkerMonitor] 心跳通道已关闭");
                    break;
                }
            }
            // 定期检查心跳超时
            _ = heartbeat_check.tick() => {
                if worker_manager.check_heartbeat_timeout() {
                    error!("❌ [WorkerMonitor] 心跳超时，agent_worker 可能已崩溃");
                    // 重启并获取新的通道
                    let (new_heartbeat_rx, new_ready_rx) = restart_worker(worker_manager.clone()).await?;
                    heartbeat_rx = new_heartbeat_rx;

                    // 🆕 等待重启后的 Worker 发送就绪信号（oneshot）
                    match timeout(Duration::from_secs(30), new_ready_rx).await {
                        Ok(Ok(_ready)) => {
                            worker_manager.update_state(agent_worker_manager::WorkerState::Running);
                            info!("✅ [WorkerMonitor] 重启后 Worker 就绪，状态已更新为 Running");
                        }
                        _ => {
                            error!("❌ [WorkerMonitor] 重启后 Worker 启动超时");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// 🔥 重启 worker 线程
///
/// 创建新的通道和线程，并更新 manager 的 sender
///
/// 🆕 优化：返回 (heartbeat_rx, ready_rx)，状态由监控任务在收到 Ready 信号后设置
#[allow(clippy::type_complexity)]
async fn restart_worker(
    worker_manager: Arc<AgentWorkerManager>,
) -> anyhow::Result<(
    tokio::sync::mpsc::Receiver<agent_worker_manager::Heartbeat>,
    tokio::sync::oneshot::Receiver<agent_worker_manager::WorkerReady>,
)> {
    info!("🔄 [WorkerMonitor] 开始重启 agent_worker...");

    // 1. 更新状态为启动中
    worker_manager.update_state(agent_worker_manager::WorkerState::Starting);

    // 🆕 2. 重置心跳时间，避免旧的过期心跳时间导致立即触发超时
    worker_manager.reset_heartbeat();

    // 3. 创建新的心跳通道
    let (new_heartbeat_tx, new_heartbeat_rx) = tokio::sync::mpsc::channel(100);

    // 3. 创建新的就绪信号通道（oneshot）
    let (new_ready_tx, new_ready_rx) = tokio::sync::oneshot::channel();

    // 4. 创建新的任务通道
    let (new_sender, new_receiver) = tokio::sync::mpsc::unbounded_channel();

    // 5. 创建新的 worker handle（包含心跳和就绪通道）
    let worker_handle = worker_manager.create_handle(new_heartbeat_tx, new_ready_tx);

    // 6. 启动新的 worker 线程
    std::thread::spawn(move || {
        if let Err(e) = run_agent_worker_thread(new_receiver, worker_handle) {
            error!("❌ [WorkerThread] 重启的 agent_worker 崩溃: {}", e);
        }
    });

    // 7. 原子替换 sender
    worker_manager.replace_sender(new_sender);

    // 🆕 不再立即设置 Running 状态，等待 Ready 信号后由 monitor_worker_health 设置

    info!("🔄 [WorkerMonitor] agent_worker 线程已启动，等待就绪信号...");

    // 返回新的 heartbeat_rx 和 ready_rx
    Ok((new_heartbeat_rx, new_ready_rx))
}
