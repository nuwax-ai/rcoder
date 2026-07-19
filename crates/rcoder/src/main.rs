mod background_tasks;
mod batch_migrate;
mod bootstrap;
mod cleanup_task;
mod config;
mod config_watcher;
mod docker_init;
mod file_server_embed;
mod handler;
mod middleware;
mod proxy_init;
mod router;
mod server;
mod service;
mod shutdown;
mod utils;
mod workspace_migrate;

use std::sync::Arc;

use tracing::{info, warn};

use rcoder::*;

use router::AppState;

use docker_manager::runtime_selection::RuntimeType;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Feature 开关: 启动读一次 env + eprintln 打印状态 (console, tracing 未就绪也可见)
    shared_types::FeatureFlags::init();

    // 版本标识 (先 eprintln 保证 console 一定输出; bootstrap 后再 info! 写文件日志)
    let version_line = format!(
        "🚀 rcoder v{} — BUILD: {} @ {} (branch: {})",
        env!("CARGO_PKG_VERSION"),
        env!("RCODER_BUILD_GIT_HASH"),
        env!("RCODER_BUILD_TIME"),
        env!("RCODER_BUILD_GIT_BRANCH")
    );
    eprintln!("{version_line}");

    let bootstrap_result = bootstrap::bootstrap().await?;

    // bootstrap 完成 (tracing 已初始化) → 再写一次到文件日志
    info!("{version_line}");
    info!(target: "feature_flags", "{:?}", shared_types::FeatureFlags::get());

    let runtime_type = RuntimeType::from_env();
    let is_kubernetes = shared_types::is_kubernetes_runtime();
    info!(
        "Runtime type: {:?}, is_kubernetes_runtime: {}",
        runtime_type, is_kubernetes
    );
    info!(
        "🔧 [STARTUP] Container runtime: {}",
        if is_kubernetes {
            "Kubernetes"
        } else {
            "Docker"
        }
    );

    docker_init::init_path_resolver(runtime_type).await?;
    docker_init::init_docker_manager(&bootstrap_result.config).await?;
    docker_init::startup_cleanup(&bootstrap_result.config).await;

    // 提前创建 ProjectAdapter，以便同一 Arc 实例同时作为
    // Arc<dyn ContainerLookup> 注入 Pingora 代理层（统一容器 IP 数据源）
    // 和作为 AppState.projects 共享给业务逻辑。
    let cluster_domain = shared_types::get_k8s_cluster_domain();
    let (projects, cleanup_rx) = ProjectAdapter::new(
        bootstrap_result.config.app_manager.namespace.clone(),
        cluster_domain.clone(),
    );
    let projects = Arc::new(projects);

    // 克隆同一 Arc 实例供 Pingora 代理层使用（共享 DashMap 数据）。
    // 必须先得到具体类型 Arc<ProjectAdapter>，再在其上做 unsized coercion
    // 到 trait object，避免类型推断把 clone 的类型参数反向绑定为 dyn。
    let projects_for_lookup = Arc::clone(&projects);
    let container_lookup: Arc<dyn shared_types::ContainerLookup> = projects_for_lookup;

    let proxy_result = proxy_init::init_proxy(
        &bootstrap_result.config,
        Arc::clone(&bootstrap_result.api_key_config),
        container_lookup,
    )
    .await;
    proxy_init::log_proxy_info(&bootstrap_result.config);

    let shutdown_tx = shutdown::setup_signal_handlers();

    let _config_watcher = if bootstrap_result.config_watcher_enabled {
        match config_watcher::ConfigWatcher::new(
            bootstrap_result.config_file_path.clone(),
            Arc::clone(&bootstrap_result.api_key_config),
        ) {
            Ok(watcher) => {
                info!(
                    "📁 Config file watcher started: {:?}",
                    bootstrap_result.config_file_path
                );
                Some(watcher)
            }
            Err(e) => {
                warn!("config file watcher start failed: {}, API Key updated", e);
                None
            }
        }
    } else {
        None
    };

    let (container_prefix_rcoder, container_prefix_computer) =
        docker_init::get_container_prefixes(&bootstrap_result.config)?;

    // 获取容器运行时（在 init_docker_manager 之后可用）
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get container runtime: {}", e))?;

    // 阶段2 批量迁移: 启动后台 task 将共享 PVC 老数据一次性迁到 per-agent PVC (env 开关, 默认 false)
    batch_migrate::spawn_if_enabled(runtime.clone());

    // 阶段2 方案C: rcoder 同进程嵌入 file-server (FeatureFlags.embed_file_server, 灰度)。
    // 启用时 rcoder 进程内 spawn file-server (端口 60000), 经 SubvolumeWorkspaceResolver
    // 复用本进程 ContainerRuntime 解析 per-agent subvolume 聚合路径 (file-server 不加 kube 依赖)。
    // 配套: start-services.sh 须检查本 env, 嵌入时不再单独启 file-server 二进制 (避免端口冲突)。
    if shared_types::FeatureFlags::get().embed_file_server {
        file_server_embed::spawn_embedded_file_server(Arc::clone(&runtime)).await;
    }

    let state = Arc::new(
        AppState::new(
            bootstrap_result.config.clone(),
            proxy_result.pingora_service.clone(),
            bootstrap_result.api_key_config,
            container_prefix_rcoder,
            container_prefix_computer,
            runtime,
            projects,
            cleanup_rx,
        )
        .await?,
    );

    let _bg_handles =
        background_tasks::start_all_background_tasks(&bootstrap_result.config, state.clone())
            .await?;

    let runtime_for_shutdown = state.runtime().clone();
    let app = router::create_router(state, Some(bootstrap_result.telemetry));
    let server_handle =
        server::start_http_server(app, bootstrap_result.config.port, shutdown_tx.clone()).await?;

    shutdown::graceful_shutdown(
        shutdown_tx.subscribe(),
        bootstrap_result.config.clone(),
        runtime_for_shutdown,
    )
    .await;
    server_handle.abort();

    if let Some(pingora_shutdown_tx) = proxy_result.pingora_shutdown_tx {
        let _ = pingora_shutdown_tx.send(());
    }
    if let Some(proxy_handle) = proxy_result.proxy_handle {
        let _ = proxy_handle.await;
    }

    Ok(())
}
