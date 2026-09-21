//! rcoder 库
//!
//! Phase 0 后 rcoder = HTTP 面（handler/router/middleware/server，Phase 1 迁
//! http-server）+ 组合根（main.rs 经 `rcoder::` 引用）。引擎模块已整体迁
//! `rcoder-engine` crate，此处 re-export 保持 `crate::X` 路径兼容——HTTP 层
//! 与 bin 的既有引用零改动，行为零变化。
/// dial9 事件级 Tokio tracing 装配（`dial9` feature 专用；bin 的 main 手动
/// 构建 runtime 时经 `rcoder::dial9_obs` 调用，须 pub）
#[cfg(feature = "dial9")]
pub use rcoder_engine::dial9_obs;
pub use rcoder_engine::{
    app_state, background_tasks, batch_migrate, bootstrap, cleanup_task, config, config_watcher,
    docker_init, file_server_admin, file_server_embed, grpc, http_client, preview_assembly,
    proxy_init, service, shutdown, skill_sync_reconciler, storage, userapp_builder,
    userapp_forward, userapp_recycle, utils, vnc, workspace_migrate,
};

// HTTP 面（Phase 1 迁 http-server crate 后 re-export）
pub use http_server::{handler, middleware, router, router_docs, server};

// 重新导出主要的类型和函数
pub use storage::{ProjectAdapter, ProjectStore, ProjectStoreBackend};
pub use utils::*;

// 重新导出 shared_types 中的类型
pub use shared_types::{
    AgentSessionUpdate, AgentStatus, AgentStatusResponse, AppError, Attachment, AttachmentError,
    AttachmentSource, AudioAttachment, CancelNotificationResponse, ChatPrompt, ChatPromptResponse,
    ChatResponse, DocumentAttachment, HttpResult, ImageAttachment, ImageDimensions,
    ModelProviderConfig, ModelProviderSafeInfo, ProjectAndAgentInfo, SessionMessageType,
    SessionNotify, SessionPromptEnd, SessionPromptStart, TextAttachment, UnifiedSessionMessage,
};

/// 服务组合入口：CLI 探测后的完整启动序（bootstrap → 引擎装配 → 路由挂载 →
/// serve → 优雅关停）。bin 的两个变体（dial9 手动 runtime / `#[tokio::main]`）
/// 都经此入口；desktop（Phase 6）复用 `rcoder_engine::assemble` 同一装配序。
///
/// 引擎装配序（docker_init/存储/Pingora/AppState/后台任务，含三处 ArcSwap 槽
/// 回填的晚绑定协议）收敛在 `rcoder_engine::assemble`——为何必须晚绑定见该
/// 模块文档（构造环 + 数据平面先行 + 失败隔离）。
pub async fn run() -> anyhow::Result<()> {
    use std::sync::Arc;

    // Feature 开关: 启动读一次 env + eprintln 打印状态 (console, tracing 未就绪也可见)
    shared_types::FeatureFlags::init();

    // Hotpath tokio runtime 指标采样线程（feature 关闭时 no-op）
    hotpath::tokio_runtime!();

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
    tracing::info!("{version_line}");
    tracing::info!(target: "feature_flags", "{:?}", shared_types::FeatureFlags::get());

    let engine = rcoder_engine::assemble::assemble(bootstrap_result).await?;
    let rcoder_engine::assemble::AssembledEngine {
        state,
        merged_fs,
        proxy_result,
        bg_handles,
        shutdown_tx,
        mut shutdown_rx,
        config,
        telemetry,
        runtime_for_shutdown,
        projects_for_shutdown,
        userapp_store_control,
        activity_for_shutdown,
        userapp_op_flight,
        userapp_recovery,
        _config_watcher,
    } = engine;

    let app = router::create_router(state, Some(Arc::clone(&telemetry)), merged_fs);
    let server_handle = server::start_http_server(app, config.port, shutdown_tx.clone()).await?;

    let _ = shutdown_rx.recv().await;
    let deadline = tokio::time::Instant::now() + shutdown::SHUTDOWN_BUDGET;
    userapp_op_flight.close();
    // Tasks created after the first signal still need the shutdown notification.
    let _ = shutdown_tx.send(());
    if let Some(tx) = proxy_result.pingora_shutdown_tx {
        let _ = tx.send(());
    }
    // Both producers receive shutdown before either is awaited. Detached business
    // tasks keep their admission guards through durable terminal publication.
    tokio::time::timeout_at(deadline, server_handle)
        .await
        .map_err(|_| anyhow::anyhow!("HTTP drain timed out; storage remains owned"))??;
    if let Some(handle) = proxy_result.proxy_handle {
        tokio::time::timeout_at(deadline, handle)
            .await
            .map_err(|_| anyhow::anyhow!("proxy drain timed out; storage remains owned"))??;
    }
    tokio::time::timeout_at(deadline, file_server_proxy::stop())
        .await
        .map_err(|_| anyhow::anyhow!("file-server proxy drain timed out; storage remains owned"))?
        .map_err(anyhow::Error::msg)?;
    bg_handles.drain(deadline).await?;
    shutdown::graceful_shutdown(
        deadline,
        config.clone(),
        runtime_for_shutdown,
        Some(projects_for_shutdown),
        activity_for_shutdown,
        shutdown::UserAppShutdown {
            store: userapp_store_control,
            operations: Some(userapp_op_flight),
            recovery: userapp_recovery,
        },
    )
    .await?;

    Ok(())
}
