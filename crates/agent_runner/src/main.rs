use clap::Parser;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

// 🆕 使用共享的遥测模块
use rcoder_telemetry::{TelemetryConfig, TelemetryGuard};

mod agent_worker_manager;
mod api_key_manager;
mod config;
mod grpc;
mod handler;
mod model;
mod proxy_agent;

// 🔥 OpenTelemetry 追踪模块（可选：保留用于向后兼容）
#[allow(dead_code)]
mod otel_tracing;

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
    // 🆕 初始化遥测系统（使用 rcoder-telemetry，包含控制台 + 文件日志）
    let telemetry_config = TelemetryConfig::from_env("agent_runner").with_file_log("agent-runner"); // 启用文件日志，前缀为 agent-runner
    let telemetry: TelemetryGuard = rcoder_telemetry::init(telemetry_config).await?;
    let telemetry = Arc::new(telemetry);

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

    // 创建路由（传入遥测 guard 用于 /metrics 端点）
    let app = router::create_router(state.clone(), Some(telemetry.clone()));

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

/// 🔥 新增：agent_worker 线程启动函数
///
/// 🆕 改进：使用多线程运行时 + 每个请求独立的 LocalSet
/// 支持并发处理多个 Agent 请求，避免单线程阻塞
/// 因为 ACP 连接不是 Send，每个请求在独立的 LocalSet 中运行
fn run_agent_worker_thread(
    receiver: tokio::sync::mpsc::UnboundedReceiver<proxy_agent::LocalSetAgentRequest>,
    handle: agent_worker_manager::WorkerHandle,
) -> anyhow::Result<()> {
    info!("🚀 [agent_worker_thread] 线程启动，创建多线程运行时以支持并发 Agent...");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(10) // 使用 10 个工作线程，支持更高并发处理多个 Agent
        .thread_name("agent-worker")
        .enable_all()
        .build()
        .expect("Failed to build multi-thread runtime for LocalSet agents");

    rt.block_on(async move {
        info!("🚀 [agent_worker_thread] 多线程运行时已启动，准备并发处理 Agent 请求...");

        // 🆕 不再使用全局 LocalSet，改为每个请求创建独立的 LocalSet
        // 直接调用 agent_worker_with_heartbeat，它会为每个请求 spawn 独立任务
        match proxy_agent::agent_worker_with_heartbeat(receiver, handle).await {
            Ok(_) => {
                info!("✅ [agent_worker_thread] Agent worker 正常退出");
            }
            Err(e) => {
                error!("❌ [agent_worker_thread] Agent worker 失败: {}", e);
            }
        }

        warn!("⚠️ [agent_worker_thread] Agent worker stopped");
        Ok::<(), anyhow::Error>(())
    })
}

/// 🔥 新增：监控 worker 健康状态
///
/// 监听心跳信号，检测 worker 是否崩溃，并自动重启
///
/// 🆕 优化：
/// - 增加 ready_rx 参数，等待 Worker 发送就绪信号后才设置 Running 状态
/// - 添加重试逻辑，restart_worker 失败时不会导致监控任务退出
/// - 添加连续失败计数、指数退避和冷却时间
async fn monitor_worker_health(
    worker_manager: Arc<AgentWorkerManager>,
    mut heartbeat_rx: tokio::sync::mpsc::Receiver<agent_worker_manager::Heartbeat>,
    ready_rx: tokio::sync::oneshot::Receiver<agent_worker_manager::WorkerReady>,
) -> anyhow::Result<()> {
    use tokio::time::{Duration, interval, sleep, timeout};

    const MAX_RESTART_ATTEMPTS: u32 = 5; // 最大连续重启次数
    const RESTART_COOLDOWN_SECS: u64 = 60; // 冷却时间（秒）
    const RESTART_BACKOFF_BASE_SECS: u64 = 2; // 指数退避基数（秒）

    let mut consecutive_failures: u32 = 0;
    let mut last_successful_start = std::time::Instant::now();

    info!("🔍 [WorkerMonitor] 健康监控任务已启动，等待 Worker 就绪...");

    // 🆕 等待初始 Worker 就绪信号（带 30 秒超时）
    match timeout(Duration::from_secs(30), ready_rx).await {
        Ok(Ok(_ready)) => {
            worker_manager.update_state(agent_worker_manager::WorkerState::Running);
            last_successful_start = std::time::Instant::now();
            info!("✅ [WorkerMonitor] Worker 就绪，状态已更新为 Running");
        }
        Ok(Err(_)) => {
            consecutive_failures += 1;
            error!("❌ [WorkerMonitor] Ready 通道意外关闭，将触发重启");
            // 尝试重启
            if let Ok((new_hb_rx, new_ready_rx)) = restart_worker(worker_manager.clone()).await {
                heartbeat_rx = new_hb_rx;
                if let Ok(Ok(_)) = timeout(Duration::from_secs(30), new_ready_rx).await {
                    worker_manager.update_state(agent_worker_manager::WorkerState::Running);
                    consecutive_failures = 0;
                    last_successful_start = std::time::Instant::now();
                    info!("✅ [WorkerMonitor] 重启后 Worker 就绪");
                }
            }
        }
        Err(_) => {
            consecutive_failures += 1;
            error!("❌ [WorkerMonitor] Worker 启动超时（30 秒未收到就绪信号），将触发重启");
            // 尝试重启
            if let Ok((new_hb_rx, new_ready_rx)) = restart_worker(worker_manager.clone()).await {
                heartbeat_rx = new_hb_rx;
                if let Ok(Ok(_)) = timeout(Duration::from_secs(30), new_ready_rx).await {
                    worker_manager.update_state(agent_worker_manager::WorkerState::Running);
                    consecutive_failures = 0;
                    last_successful_start = std::time::Instant::now();
                    info!("✅ [WorkerMonitor] 重启后 Worker 就绪");
                }
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
            // 定期检查心跳超时和请求超时
            _ = heartbeat_check.tick() => {
                // 🔥 检查心跳超时
                if worker_manager.check_heartbeat_timeout() {
                    error!("❌ [WorkerMonitor] 心跳超时，agent_worker 可能已崩溃");

                    // 🆕 检查是否需要冷却
                    if consecutive_failures >= MAX_RESTART_ATTEMPTS {
                        let elapsed = last_successful_start.elapsed();
                        if elapsed < Duration::from_secs(RESTART_COOLDOWN_SECS) {
                            let remaining = RESTART_COOLDOWN_SECS - elapsed.as_secs();
                            warn!(
                                "⏳ [WorkerMonitor] 连续重启失败 {} 次，进入冷却期（剩余 {} 秒）",
                                consecutive_failures, remaining
                            );
                            continue; // 跳过本次重启，等待冷却期
                        } else {
                            // 冷却期结束，重置计数
                            info!("🔄 [WorkerMonitor] 冷却期结束，重置失败计数");
                            consecutive_failures = 0;
                        }
                    }

                    // 🆕 指数退避等待
                    if consecutive_failures > 0 {
                        let backoff = RESTART_BACKOFF_BASE_SECS.pow(consecutive_failures);
                        let backoff = backoff.min(30); // 最大 30 秒
                        warn!(
                            "⏳ [WorkerMonitor] 等待 {} 秒后重试（第 {} 次重试）",
                            backoff, consecutive_failures + 1
                        );
                        sleep(Duration::from_secs(backoff)).await;
                    }

                    // 尝试重启
                    match restart_worker(worker_manager.clone()).await {
                        Ok((new_hb_rx, new_ready_rx)) => {
                            heartbeat_rx = new_hb_rx;
                            // 等待 Ready 信号
                            match timeout(Duration::from_secs(30), new_ready_rx).await {
                                Ok(Ok(_ready)) => {
                                    worker_manager.update_state(agent_worker_manager::WorkerState::Running);
                                    consecutive_failures = 0;
                                    last_successful_start = std::time::Instant::now();
                                    info!("✅ [WorkerMonitor] 重启后 Worker 就绪，状态已更新为 Running");
                                }
                                _ => {
                                    consecutive_failures += 1;
                                    error!("❌ [WorkerMonitor] 重启后 Worker 启动超时（第 {} 次失败）", consecutive_failures);
                                }
                            }
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            error!("❌ [WorkerMonitor] restart_worker 失败（第 {} 次）: {}", consecutive_failures, e);
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

    // 4. 创建新的就绪信号通道（oneshot）
    let (new_ready_tx, new_ready_rx) = tokio::sync::oneshot::channel();

    // 5. 创建新的任务通道
    let (new_sender, new_receiver) = tokio::sync::mpsc::unbounded_channel();

    // 6. 创建新的 worker handle（包含心跳和就绪通道）
    let worker_handle = worker_manager.create_handle(new_heartbeat_tx, new_ready_tx);

    // 7. 启动新的 worker 线程
    std::thread::spawn(move || {
        if let Err(e) = run_agent_worker_thread(new_receiver, worker_handle) {
            error!("❌ [WorkerThread] 重启的 agent_worker 崩溃: {}", e);
        }
    });

    // 8. 原子替换 sender
    worker_manager.replace_sender(new_sender);

    // 🆕 不再立即设置 Running 状态，等待 Ready 信号后由 monitor_worker_health 设置

    info!("🔄 [WorkerMonitor] agent_worker 线程已启动，等待就绪信号...");

    // 返回新的 heartbeat_rx 和 ready_rx
    Ok((new_heartbeat_rx, new_ready_rx))
}
