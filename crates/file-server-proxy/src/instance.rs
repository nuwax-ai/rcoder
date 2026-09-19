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
    finished: tokio::sync::watch::Receiver<Option<Result<(), String>>>,
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
    if guard
        .as_ref()
        .is_some_and(|i| i.finished.borrow().is_some())
    {
        guard.take();
        warn!("file-server 分流代理 serve 已退出, 死实例状态自愈为已停止");
    }
}

/// 实例锁文件路径：用户稳定目录下按「监听语义」分锁域——固定端口一个
/// 锁域；动态端口（0）每实例独立域（多实例并行合法）。
fn state_directory(
    explicit: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    profile: Option<std::ffi::OsString>,
) -> Result<std::path::PathBuf, String> {
    if let Some(path) = explicit.filter(|value| !value.is_empty()) {
        return Ok(path.into());
    }
    home.filter(|value| !value.is_empty())
        .or_else(|| profile.filter(|value| !value.is_empty()))
        .map(|path| std::path::PathBuf::from(path).join(".file-server-proxy"))
        .ok_or_else(|| {
            "set FILE_SERVER_PROXY_STATE_DIR when HOME and USERPROFILE are unavailable".into()
        })
}

fn listener_address(host: &str, port: u16) -> String {
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => std::net::SocketAddr::new(ip, port).to_string(),
        Err(_) => format!("{host}:{port}"),
    }
}

fn instance_lock_path(config: &FileServerProxyConfig) -> Result<std::fs::File, String> {
    let dir = state_directory(
        std::env::var_os("FILE_SERVER_PROXY_STATE_DIR"),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
    )?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create state dir {}: {e}", dir.display()))?;
    let port_key = if config.listen_port == 0 {
        // 动态端口：per-invocation 独立锁（uuid 后缀），不互斥
        format!("dynamic-{}", std::process::id())
    } else {
        // Encode configuration bytes, rather than placing IPv6 colons or path
        // separators in a Windows filename.
        let host_key: String = config
            .listen_host
            .trim()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{host_key}-{}", config.listen_port)
    };
    let path = dir.join(format!("instance-{port_key}.lock"));
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("open instance lock {}: {e}", path.display()))
}

/// 测试探针：锁域路径解析（不持锁）。
#[cfg(test)]
pub(crate) fn test_lock_file(config: &FileServerProxyConfig) -> Result<std::fs::File, String> {
    instance_lock_path(config)
}

/// 启动分流代理（幂等）。同步 bind（而非 spawn 内 bind），返回时状态准确。
pub async fn try_start() -> Result<String, String> {
    let mut guard = INSTANCE.lock().await;
    reap_dead_instance(&mut guard);
    if let Some(instance) = guard.as_ref() {
        if instance.shutdown.is_cancelled() {
            return Err("file-server proxy is still stopping".into());
        }
        return Ok(instance.address.clone());
    }
    // N06：跨进程实例锁——同配置空间（listen host:port 语义）单实例；
    // 锁文件随进程退出释放（std 文件锁）。JS 启动器的 PID 文件只是
    // 观察线索，不再是归属权威——锁竞争方在此处决出唯一胜者。
    let config_for_lock = CONFIG.get().cloned().unwrap_or_default();
    let lock_file = instance_lock_path(&config_for_lock)?;
    let config = CONFIG.get().cloned().unwrap_or_else(|| {
        warn!("file-server-proxy 配置未 init, 回落默认端口 (60000 → 8086/60001)");
        FileServerProxyConfig::default()
    });

    // N07（修订）：非 loopback + 无令牌 + **显式 public_bind_declared:false**
    // 才拒启——默认受管放行（09-19 实战结论：本服务部署形态以容器/编排为
    // 主流，网络边界由使用方在编排层控制；原"默认严格+各处声明"的门只挡
    // 自己人）。使用方收紧通道：config 显式 false、HOST=127.0.0.1 或令牌。
    let host = config.listen_host.trim();
    let is_loopback = matches!(host, "127.0.0.1" | "::1" | "localhost");
    if !is_loopback && config.auth_token.is_none() && !config.public_bind_declared {
        return Err(format!(
            "refusing to listen on non-loopback {host} without FILE_SERVER_PROXY_TOKEN \
             (public_bind_declared is explicitly false — set the token, bind 127.0.0.1, \
             or remove the explicit false to allow managed public bind)"
        ));
    }

    // N03：host 可配（原生 loopback / 容器 0.0.0.0）；端口 0 = 动态分配，
    // 绑定后以 local_addr 真实地址为准（不返回 ":0"）
    let requested = listener_address(host, config.listen_port);
    let listener = tokio::net::TcpListener::bind(&requested)
        .await
        .map_err(|e| format!("bind {requested} 失败（端口被占用?）: {e}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read bound proxy listener address: {error}"))?
        .to_string();
    // 持锁（bind 已成功——锁与端口双占有）：句柄存进 RunningInstance，
    // stop 时 drop 释放（同进程 stop+restart 可重新获锁）；进程崩溃内核释放
    lock_file
        .try_lock()
        .map_err(|e| format!("another file-server-proxy instance holds the lock: {e}"))?;

    // 上游 hang 防堆积: 连接 5s 建立超时; 整请求超时在 proxy_request 内包装
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_connect_timeout(Some(std::time::Duration::from_secs(5)));
    let client: ProxyClient =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(connector);

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let listening_addr = address.clone();
    let (finished_tx, finished) = tokio::sync::watch::channel(None);
    let task = tokio::spawn(async move {
        // The task, not a stop caller, owns the lock through connection drain.
        let _lock = lock_file;
        info!(
            "file-server 分流代理运行中 ({listening_addr}; {USERAPP_PATH_PREFIX}* 或 \
             {SERVICE_TYPE_HEADER}: {SERVICE_TYPE_USERAPP} → 127.0.0.1:{}, 其余 → 127.0.0.1:{})",
            config.rust_upstream_port, config.ts_upstream_port
        );
        serve(listener, client, config, token).await
    });

    tokio::spawn(async move {
        let result = task
            .await
            .map_err(|error| format!("proxy serve task failed: {error}"))
            .and_then(std::convert::identity);
        finished_tx.send_replace(Some(result));
    });
    info!("file-server 分流代理启动: {address}");
    *guard = Some(RunningInstance {
        shutdown,
        finished,
        address: address.clone(),
    });
    Ok(address)
}

/// Stop is shared and cancellation-safe. New starts cannot bypass the draining instance.
pub async fn stop() -> Result<(), String> {
    let mut finished = {
        let guard = INSTANCE.lock().await;
        let Some(instance) = guard.as_ref() else {
            return Ok(());
        };
        instance.shutdown.cancel();
        instance.finished.clone()
    };
    loop {
        if let Some(result) = finished.borrow_and_update().clone() {
            return result;
        }
        finished
            .changed()
            .await
            .map_err(|_| "proxy shutdown result channel closed".to_owned())?;
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn standalone_state_uses_windows_profile_without_home() {
        let profile = std::ffi::OsString::from("profile");
        assert_eq!(
            state_directory(None, None, Some(profile.clone())).unwrap(),
            std::path::Path::new("profile").join(".file-server-proxy")
        );
        assert_eq!(
            state_directory(Some("explicit".into()), Some("home".into()), Some(profile)).unwrap(),
            std::path::PathBuf::from("explicit")
        );
        assert!(state_directory(None, Some("".into()), None).is_err());
    }

    #[test]
    fn ipv6_listener_uses_socket_address_syntax() {
        assert_eq!(listener_address("::1", 0), "[::1]:0");
        assert_eq!(listener_address("127.0.0.1", 60000), "127.0.0.1:60000");
        assert_eq!(listener_address("localhost", 60000), "localhost:60000");
    }
}
