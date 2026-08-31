//! 全局生命周期管理（admin API / CLI 复刻自 f55f230 的内嵌 file-server 模式）。

use std::sync::OnceLock;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::{
    FileServerProxyConfig, SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP, USERAPP_PATH_PREFIX,
};
use crate::proxy::{ProxyClient, serve};

// ── 全局生命周期管理（admin API / CLI 复刻自 f55f230 的内嵌 file-server 模式）──
/// 运行中的代理实例（shutdown 信号 + serve task + 监听地址）。
struct RunningInstance {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    address: String,
}

/// 配置注册（main 无条件调用，段缺失时用 Default——本地 dev 也可经 admin API 拉起）。
static CONFIG: OnceLock<FileServerProxyConfig> = OnceLock::new();

/// 当前实例（None = 未运行，60000 未被本代理占用）。
static INSTANCE: tokio::sync::Mutex<Option<RunningInstance>> = tokio::sync::Mutex::const_new(None);

/// 注册配置（幂等，首次生效）。
///
/// main 启动时调用；config.yml 无 `file_server_proxy` 段时传
/// [`FileServerProxyConfig::default`]（不自动启动，仅让运行时 `start` 可用）。
pub fn init(config: FileServerProxyConfig) {
    if CONFIG.set(config).is_err() {
        tracing::debug!("file-server-proxy config already registered, keep first");
    }
}

/// 当前运行状态：Some(address) = 运行中，None = 已停止。
///
/// 顺带自愈：serve task 已死（意外退出且 spawn 内 cleanup 被 panic 跳过等）时
/// 就地清掉死实例，避免 status 误报 running。
pub async fn status() -> Option<String> {
    let mut guard = INSTANCE.lock().await;
    reap_dead_instance(&mut guard);
    guard.as_ref().map(|i| i.address.clone())
}

/// map 内实例的 task 已结束则清掉（幂等；serve panic 跳过 spawn 内 cleanup 的兜底）。
fn reap_dead_instance(guard: &mut tokio::sync::MutexGuard<'_, Option<RunningInstance>>) {
    if guard.as_ref().is_some_and(|i| i.task.is_finished()) {
        guard.take();
        warn!("file-server 分流代理 serve 已退出, 死实例状态自愈为已停止");
    }
}

/// 启动分流代理（幂等）。同步 bind（而非 spawn 内 bind），返回时状态准确。
pub async fn try_start() -> Result<String, String> {
    let mut guard = INSTANCE.lock().await;
    reap_dead_instance(&mut guard);
    if let Some(instance) = guard.as_ref() {
        return Ok(instance.address.clone());
    }
    let config = CONFIG.get().cloned().unwrap_or_else(|| {
        warn!("file-server-proxy 配置未 init, 回落默认端口 (60000 → 8086/60001)");
        FileServerProxyConfig::default()
    });

    let address = format!("0.0.0.0:{}", config.listen_port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| format!("bind {address} 失败（端口被占用?）: {e}"))?;

    // 上游 hang 防堆积: 连接 5s 建立超时; 整请求超时在 proxy_request 内包装
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_connect_timeout(Some(std::time::Duration::from_secs(5)));
    let client: ProxyClient =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(connector);

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let listening_addr = address.clone();
    let task = tokio::spawn(async move {
        info!(
            "file-server 分流代理运行中 ({listening_addr}; {USERAPP_PATH_PREFIX}* 或 \
             {SERVICE_TYPE_HEADER}: {SERVICE_TYPE_USERAPP} → 127.0.0.1:{}, 其余 → 127.0.0.1:{})",
            config.rust_upstream_port, config.ts_upstream_port
        );
        serve(listener, client, config, token).await;
        // serve 意外退出（accept 错误等）: 清理 INSTANCE, 避免 status 误报 running
        cleanup_dead_instance().await;
    });

    info!("file-server 分流代理启动: {address}");
    *guard = Some(RunningInstance {
        shutdown,
        task,
        address: address.clone(),
    });
    Ok(address)
}

/// serve task 结束后的 INSTANCE 清理（正常 stop 已 take，此处只兜底意外退出）。
async fn cleanup_dead_instance() {
    let mut guard = INSTANCE.lock().await;
    if guard.as_ref().is_some_and(|i| i.task.is_finished()) {
        guard.take();
        warn!("file-server 分流代理实例已随 serve 退出清理（意外退出）");
    }
}

/// 停止分流代理（幂等）。
///
/// cancel → 等 serve task 结束（**10s 超时 abort + 再 await**，确保 listener drop
/// 端口释放）；返回时 60000 已可用（外部服务如 TS nuwax-file-server 可立即 bind）。
pub async fn stop() -> Result<(), String> {
    let instance = INSTANCE.lock().await.take();
    let Some(mut instance) = instance else {
        return Ok(());
    };
    // 锁已随 guard drop 释放，等 task 期间不阻塞 status/start
    instance.shutdown.cancel();
    let timeout = std::time::Duration::from_secs(10);
    if tokio::time::timeout(timeout, &mut instance.task)
        .await
        .is_err()
    {
        warn!("file-server 分流代理优雅停机超时, abort");
        instance.task.abort();
        // abort 仅调度取消, 再 await 确保 listener 已 drop（端口真正释放）;
        // 随后的 JoinError 是预期取消路径
        if let Err(join_err) = (&mut instance.task).await {
            tracing::debug!("proxy task join after abort: {join_err}");
        }
    }
    info!("file-server 分流代理已停止, 60000 端口已释放");
    Ok(())
}
