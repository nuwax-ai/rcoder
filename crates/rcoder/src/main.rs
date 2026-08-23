// 单树化后 bin 只做编排入口：全部模块经 `rcoder::`（lib 树唯一编译）。
use std::sync::Arc;

use tracing::{info, warn};

use rcoder::app_state::AppState;
use rcoder::*;

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

    // 提前创建存储后端（M4：按 config.storage 分叉 Memory/Postgres 枚举），
    // 以便同一 Arc 实例同时作为 Arc<dyn ContainerLookup> 注入 Pingora 代理层
    // （统一容器 IP 数据源）和作为 AppState.projects 共享给业务逻辑。
    let cluster_domain = shared_types::get_k8s_cluster_domain();
    let (projects_backend, cleanup_rx) = match bootstrap_result.config.storage.backend {
        config::StorageBackend::Memory => {
            let (adapter, cleanup_rx) = ProjectAdapter::new(
                bootstrap_result.config.app_manager.namespace.clone(),
                cluster_domain.clone(),
            );
            (ProjectStoreBackend::Memory(Arc::new(adapter)), cleanup_rx)
        }
        // PG 模式 fail fast：未编译 feature / 连接失败 / 迁移失败均直接退出，
        // 绝不静默降级内存（会造成 PG 与镜像分叉）
        #[cfg(feature = "rcoder-pg")]
        config::StorageBackend::Postgres => {
            let pg_config = &bootstrap_result.config.storage.postgres;
            let (store, cleanup_rx) = rcoder_storage::pg::PgStore::connect(
                pg_config,
                bootstrap_result.config.app_manager.namespace.clone(),
                cluster_domain.clone(),
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!("[STORAGE_PG] postgres backend init failed: {e:#}");
                std::process::exit(1);
            });
            (ProjectStoreBackend::Postgres(Arc::new(store)), cleanup_rx)
        }
        #[cfg(not(feature = "rcoder-pg"))]
        config::StorageBackend::Postgres => {
            eprintln!(
                "storage.backend=postgres 但二进制未编译 rcoder-pg feature；\
                 请用 --features rcoder-pg 构建，或将 RCODER_STORAGE_BACKEND 设为 memory"
            );
            std::process::exit(1);
        }
    };
    let projects = Arc::new(projects_backend);
    // 关停 flush 用的克隆（AppState::new 会 move 主 Arc）
    let projects_for_shutdown = Arc::clone(&projects);

    // M5：PG 模式的 publish 任务行持久化（与 PgStore 共池）。启动恢复：未终态任务
    // 全部标记 failed（orchestrator 随进程消亡，running 必为僵尸）。
    #[cfg(feature = "rcoder-pg")]
    let publish_repo: Option<Arc<dyn rcoder_storage::publish_repo::PublishTaskPersistence>> =
        if projects.is_postgres() {
            let ProjectStoreBackend::Postgres(store) = &*projects else {
                unreachable!("is_postgres 为真的分支");
            };
            let publish: Arc<dyn rcoder_storage::publish_repo::PublishTaskPersistence> = Arc::new(
                rcoder_storage::pg::userapp::publish::PgPublishTaskPersistence::new(
                    store.pool().clone(),
                ),
            );
            match publish
                .recover_running(
                    "rcoder restarted",
                    &userapp_publish::store::owner_pod_name(),
                    chrono::Utc::now()
                        - chrono::Duration::seconds(userapp_publish::store::STALE_TASK_SECS),
                )
                .await
            {
                Ok(n) if n > 0 => {
                    tracing::warn!(
                        "[STORAGE_PG] recovered {n} orphaned publish tasks (marked failed)"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[STORAGE_PG] publish task recovery failed: {e:#}");
                    std::process::exit(1);
                }
            }
            Some(publish)
        } else {
            None
        };
    #[cfg(not(feature = "rcoder-pg"))]
    let publish_repo: Option<Arc<dyn rcoder_storage::publish_repo::PublishTaskPersistence>> = None;

    // 克隆同一 Arc 实例供 Pingora 代理层使用（共享底层数据）。
    // 必须先得到具体类型 Arc<ProjectStoreBackend>，再在其上做 unsized coercion
    // 到 trait object，避免类型推断把 clone 的类型参数反向绑定为 dyn。
    let projects_for_lookup = Arc::clone(&projects);
    let container_lookup: Arc<dyn shared_types::ContainerLookup> = projects_for_lookup;

    // UserApp 活动状态注册表（闲置回收 + 流量唤醒的共享状态）。
    // 独立 Arc 在 Pingora 之前构造（注入代理层）；runtime 延迟到下方 RuntimeManager::get 后
    // 经 set_runtime 填充（OnceLock）——wake 只在 is_stopped 真时触发，而 stopped 表要到
    // AppService::new 才填充，此时 OnceLock 早已 set。
    let wake_timeout = std::time::Duration::from_secs(
        bootstrap_result.config.userapp_recycle.wake_timeout_seconds,
    );
    let activity_registry: Arc<app_manager::AppActivityRegistry> =
        Arc::new(app_manager::AppActivityRegistry::new(wake_timeout));
    let access_tracker: Arc<dyn shared_types::AppAccessTracker> = activity_registry.clone();
    let wake_control: Arc<dyn shared_types::AppWakeControl> = activity_registry.clone();

    // M5：PG 模式的 activity 影子持久化——加载必须早于 AppState::new（内部的
    // AppService::new/rebuild_stopped_apps 仅对未加载到的 Running app seed_accessed）
    if projects.is_postgres() {
        #[cfg(feature = "rcoder-pg")]
        {
            let ProjectStoreBackend::Postgres(store) = &*projects else {
                unreachable!("is_postgres 为真的分支");
            };
            let activity_persistence: Arc<dyn shared_types::ActivityPersistence> = Arc::new(
                rcoder_storage::pg::userapp::activity::PgActivityPersistence::new(
                    store.pool().clone(),
                ),
            );
            match activity_persistence.load_all().await {
                Ok(rows) => {
                    let count = rows.len();
                    activity_registry.apply_loaded(rows);
                    tracing::info!("[STORAGE_PG] userapp_activity loaded: {count} rows");
                }
                Err(e) => {
                    eprintln!("[STORAGE_PG] userapp_activity load failed: {e:#}");
                    std::process::exit(1);
                }
            }
            activity_registry.set_persistence(activity_persistence);
        }
    }

    let proxy_result = proxy_init::init_proxy(
        &bootstrap_result.config,
        Arc::clone(&bootstrap_result.api_key_config),
        container_lookup,
        access_tracker,
        wake_control,
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
        docker_init::get_container_prefixes(&bootstrap_result.config).await?;

    // 获取容器运行时（在 init_docker_manager 之后可用）
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get container runtime: {}", e))?;

    // 注入 runtime 到活动注册表（wake 需要 scale + 查 status；启动早期 OnceLock 为空，此处填充）。
    // trait upcasting: Arc<dyn ContainerRuntime> → Arc<dyn UserAppRuntime>（supertrait，Rust 1.86+）
    activity_registry.set_runtime(runtime.clone());

    // userApp 运行容器终端/数据库代理的 IPv4 解析回填（Docker 模式 ttyd 只 bind
    // IPv4，见 shared_types::AppRuntimeIpResolver；Pingora 已启动——ArcSwap 槽生效）。
    if let Some(pingora_service) = proxy_result.pingora_service.as_ref() {
        pingora_service.set_app_runtime_ip_resolver(Arc::new(
            proxy_init::DockerRuntimeIpResolver::new(runtime.clone()),
        ));
    }

    // 阶段2 批量迁移: 启动后台 task 将共享 PVC 老数据一次性迁到 per-agent PVC (env 开关, 默认 false)
    batch_migrate::spawn_if_enabled(runtime.clone());

    // 启动 skill sync reconciler: 后台补齐旧 workspace 缺的 fan-out 目录 (grok/pi/...),
    // 版本 marker 驱动, 已同步的 O(1) 跳过。env RCODER_SKILL_SYNC_RECONCILE_ON_STARTUP 默认 true。
    skill_sync_reconciler::spawn_skill_sync_reconciler();

    // file-server 路由合并进主服务（无独立 listener/端口；60000 让位反向代理）。
    // runtime 无条件注册, create_router 经 merged_router() 构造基础路由挂进主 Router
    // （project/computer/git/build 老路径 + SubvolumeWorkspaceResolver per-agent PVC 解析）。
    let ws_runtime: Arc<dyn container_runtime_api::WorkspaceRuntime> = runtime.clone();
    file_server_embed::register_runtime(ws_runtime);

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
            activity_registry.clone(),
            publish_repo,
        )
        .await?,
    );

    let _bg_handles = background_tasks::start_all_background_tasks(
        &bootstrap_result.config,
        state.clone(),
        shutdown_tx.clone(),
    )
    .await?;

    let runtime_for_shutdown = state.runtime().clone();
    let app = router::create_router(state, Some(Arc::clone(&bootstrap_result.telemetry)));
    let server_handle =
        server::start_http_server(app, bootstrap_result.config.port, shutdown_tx.clone()).await?;

    shutdown::graceful_shutdown(
        shutdown_tx.subscribe(),
        bootstrap_result.config.clone(),
        runtime_for_shutdown,
        Some(projects_for_shutdown),
    )
    .await;
    server_handle.abort();

    if let Some(pingora_shutdown_tx) = proxy_result.pingora_shutdown_tx {
        let _ = pingora_shutdown_tx.send(());
    }
    if let Some(proxy_handle) = proxy_result.proxy_handle
        && let Err(e) = proxy_handle.await
    {
        warn!("proxy task join failed during shutdown: {}", e);
    }

    Ok(())
}
