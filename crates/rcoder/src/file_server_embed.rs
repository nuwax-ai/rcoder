//! 阶段2 方案C: rcoder 同进程嵌入 file-server。
//!
//! [`spawn_embedded_file_server`] 在 rcoder 进程内启动 file-server axum (端口 60000),
//! 经 [`SubvolumeWorkspaceResolver`] + 本模块 [`ContainerRuntimePathResolver`]
//! (包 `Arc<dyn ContainerRuntime>::resolve_workspace_path`) 解析 per-agent CephFS
//! subvolume 聚合路径。file-server 不加 kube 依赖, K8s 能力全经 rcoder ContainerRuntime。
//!
//! env 开关 `RCODER_EMBED_FILE_SERVER=true|1` 启用 (灰度); 配套 start-services.sh
//! 须检查本 env, 嵌入时不再单独启 file-server 二进制 (避免端口冲突)。
//!
//! 运行时启停 (迁移期 Rust↔TS 切换): [`register_runtime`] + [`try_start`] + [`stop`]
//! 由 admin API (`rcoder file-server stop/start/status` 子命令) 调用, rcoder 进程
//! 不重启即可释放/重占 60000 端口。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use container_runtime_api::WorkspaceRuntime;
use file_server::error::AppResult;
use file_server::{
    Config, FileServer, SubvolumeWorkspaceResolver, WorkspacePathResolver, WorkspaceResolver,
};
use shared_types::ServiceType;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// 包 `Arc<dyn WorkspaceRuntime>` 实现 file-server 的 [`WorkspacePathResolver`] 窄 trait。
///
/// ISP 收紧 (阶段3): file-server 仅需 workspace 能力 (resolve/ensure), 不依赖 agent 容器
/// 生命周期或 UserApp Deployment —— 类型声明即编译期约束。
///
/// `resolve_workspace_path` 失败 (K8s API 抖动 / PVC 未 Bound / Docker 模式) → 返回 `None`
/// → [`SubvolumeWorkspaceResolver`] 降级到 LocalWorkspaceResolver (fail-open, 不阻断服务)。
pub struct ContainerRuntimePathResolver {
    runtime: Arc<dyn WorkspaceRuntime>,
}

impl ContainerRuntimePathResolver {
    pub fn new(runtime: Arc<dyn WorkspaceRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl WorkspacePathResolver for ContainerRuntimePathResolver {
    async fn resolve(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> AppResult<Option<PathBuf>> {
        // resolve_workspace_path 失败 → None (降级 Local), 不传播 Err
        Ok(self
            .runtime
            .resolve_workspace_path(identifier, service_type)
            .await
            .map(|opt| opt.map(PathBuf::from))
            .unwrap_or_else(|e| {
                warn!(
                    "resolve_workspace_path failed for {} ({:?}): {}, falling back to Local",
                    identifier, service_type, e
                );
                None
            }))
    }

    async fn ensure_and_resolve(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> AppResult<Option<PathBuf>> {
        use file_server::error::AppError;

        // 1. 先 resolve (cache 快): PVC 已存在?
        if let Some(base) = self.resolve(identifier, service_type).await? {
            // PVC 存在 → 检查迁移 (幂等: dst 非空跳过; 共享有数据才迁)
            run_lazy_migrate(&self.runtime, identifier, service_type).await;
            return Ok(Some(base));
        }
        // 2. PVC 不存在 → ensure + resolve (重试等 Bound) + 迁移
        self.runtime
            .ensure_workspace(identifier, service_type, None)
            .await
            .map_err(|e| AppError::system(format!("ensure_workspace: {e}")))?;
        // ensure 后 PVC 刚创建, ceph-csi provision 异步 (volumeName/subvolumePath 填充延迟)
        // 必须重试 resolve 等 Bound, 否则首次 None → fallback Local, 后续 Some → per-agent
        // → create-project 写 Local, git 读 per-agent, 路径不一致
        const MAX_RETRIES: u32 = 30;
        let mut base: Option<PathBuf> = None;
        for attempt in 0..MAX_RETRIES {
            // 直接调 runtime.resolve_workspace_path (不经 self.resolve 吞 Err)
            // self.resolve 把 Err → Ok(None) → 重试循环误判 Docker 模式直接 break
            match self
                .runtime
                .resolve_workspace_path(identifier, service_type)
                .await
            {
                Ok(Some(path)) => {
                    if attempt > 0 {
                        info!(
                            "[ensure_and_resolve] {} PVC Bound after {} retries",
                            identifier, attempt
                        );
                    }
                    base = Some(PathBuf::from(path));
                    break;
                }
                Ok(None) => break, // 真 Docker 模式 (runtime 无聚合视角)
                Err(e) => {
                    if attempt + 1 < MAX_RETRIES {
                        tracing::debug!(
                            "[ensure_and_resolve] {} PVC pending (attempt {}/{}): {}",
                            identifier,
                            attempt + 1,
                            MAX_RETRIES,
                            e
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    } else {
                        warn!(
                            "[ensure_and_resolve] {} PVC resolve timeout after {} retries: {}, fallback Local",
                            identifier, MAX_RETRIES, e
                        );
                    }
                }
            }
        }
        let Some(base) = base else {
            return Ok(None);
        };
        run_lazy_migrate(&self.runtime, identifier, service_type).await;
        Ok(Some(base))
    }
}

/// 按 service_type 算 lazy_migrate 参数并执行 (async)。
async fn run_lazy_migrate(
    runtime: &Arc<dyn WorkspaceRuntime>,
    identifier: &str,
    service_type: &ServiceType,
) {
    let (pvc_env, subpath, dst_at_root) = match service_type {
        ServiceType::WebAgentRunner => ("RCODER_WORKSPACE_PVC_NAME", vec!["workspace"], false),
        ServiceType::ComputerAgentRunner => ("RCODER_COMPUTER_WORKSPACE_PVC_NAME", vec![], true),
        _ => return, // UserApp 不经 file-server (app_manager 直管)
    };
    // lazy_migrate 取 Arc by-value (trait upcast 需按值), clone 廉价 (原子计数).
    crate::workspace_migrate::lazy_migrate(
        Arc::clone(runtime),
        pvc_env,
        &subpath,
        identifier,
        service_type,
        identifier,
        dst_at_root,
    )
    .await;
}

/// 运行中的内嵌 file-server 实例 (shutdown 信号 + serve task + 监听地址)。
struct EmbeddedInstance {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    address: String,
}

/// ContainerRuntime 全局注册 (main 无条件调用; embed flag 关闭时也存,
/// 供运行时 `rcoder file-server start` 拉起)。
static RUNTIME: OnceLock<Arc<dyn WorkspaceRuntime>> = OnceLock::new();

/// 当前实例 (None = 未运行)。
static INSTANCE: tokio::sync::Mutex<Option<EmbeddedInstance>> = tokio::sync::Mutex::const_new(None);

/// 注册 ContainerRuntime (幂等, 首次生效; 重复注册保留首个)。
pub fn register_runtime(runtime: Arc<dyn WorkspaceRuntime>) {
    if RUNTIME.set(runtime).is_err() {
        tracing::debug!("workspace runtime already registered, keep first");
    }
}

/// 当前运行状态: Some(address) = 运行中, None = 已停止。
pub async fn status() -> Option<String> {
    INSTANCE.lock().await.as_ref().map(|i| i.address.clone())
}

/// 启动内嵌 file-server (幂等)。
/// 同步 bind (而非 spawn 内 bind), 返回时状态准确; 供启动流程与运行时 admin API 共用。
/// `port_override`: CLI/API 显式指定端口 (优先级最高, 覆盖 env FILE_SERVER_PORT/PORT)。
pub async fn try_start(port_override: Option<u16>) -> Result<String, String> {
    let mut guard = INSTANCE.lock().await;
    if let Some(instance) = guard.as_ref() {
        // 已运行: 显式指定了不同端口时明确报错 (静默吞参数会误导调用方)
        if let Some(port) = port_override {
            let current = instance.address.rsplit(':').next().unwrap_or_default();
            if current != port.to_string() {
                return Err(format!(
                    "already running on {}; stop first to change port",
                    instance.address
                ));
            }
        }
        return Ok(instance.address.clone());
    }
    let Some(runtime) = RUNTIME.get().cloned() else {
        return Err(
            "workspace runtime not registered (embedded file-server disabled at startup)"
                .to_string(),
        );
    };

    let path_resolver = Arc::new(ContainerRuntimePathResolver::new(runtime));
    let fs_resolver: Arc<dyn WorkspaceResolver> =
        Arc::new(SubvolumeWorkspaceResolver::new(path_resolver));

    let mut fs_config = Config::load().map_err(|e| format!("load file-server config: {e:#}"))?;
    // 端口优先级: args (显式覆盖) > env (Config::load 内部已处理) > 默认
    if let Some(port) = port_override {
        fs_config.port = port;
    }
    let address = format!("{}:{}", fs_config.listen_host, fs_config.port);
    let deployment_mode = fs_config.deployment_mode;
    let fs_server = FileServer::builder(fs_config)
        .with_workspace_resolver(fs_resolver)
        .build()
        .map_err(|e| format!("build embedded file-server: {e:#}"))?;

    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| format!("bind {address}: {e}"))?;

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let listening_addr = address.clone();
    let task = tokio::spawn(async move {
        info!("file-server (embedded) listening on {}", listening_addr);
        let result = fs_server
            .serve_with_shutdown(listener, async move { token.cancelled().await })
            .await;
        if let Err(e) = &result {
            warn!("embedded file-server serve exited: {e:#}");
        }
        // serve 意外退出 (错误/panic 恢复): 清理 INSTANCE, 避免脏状态
        // (status 误报 running + try_start 幂等分支返回已死地址)
        cleanup_dead_instance().await;
    });
    info!(
        "file-server (embedded) starting on {} (deployment mode: {:?})",
        address, deployment_mode
    );
    *guard = Some(EmbeddedInstance {
        shutdown,
        task,
        address: address.clone(),
    });
    Ok(address)
}

/// serve task 结束后的 INSTANCE 清理 (正常 stop 已 take, 此处只兜底意外退出)。
async fn cleanup_dead_instance() {
    let mut guard = INSTANCE.lock().await;
    // task 已结束: 若 INSTANCE 仍有值即本 task 的残留 (stop 路径会先 take 走;
    // 新 start 放入的实例 task 尚未结束, 不会误删)
    if guard.as_ref().is_some_and(|i| i.task.is_finished()) {
        guard.take();
        warn!("embedded file-server instance cleaned up after unexpected exit");
    }
}

/// 停止内嵌 file-server (幂等)。
/// cancel → 等 serve task 结束 (**10s 超时 abort + 再 await**, 确保 listener drop 端口释放);
/// 返回时端口已释放, 外部服务 (如 TS nuwax-file-server) 可立即 bind。
pub async fn stop() -> Result<(), String> {
    let instance = INSTANCE.lock().await.take();
    let Some(mut instance) = instance else {
        return Ok(());
    };
    // 锁已释放 (guard 在上一行 drop), 等 task 期间不阻塞 status/start
    instance.shutdown.cancel();
    let timeout = std::time::Duration::from_secs(10);
    if tokio::time::timeout(timeout, &mut instance.task)
        .await
        .is_err()
    {
        warn!("embedded file-server graceful stop timed out, aborting");
        instance.task.abort();
        // abort 仅调度取消, 再 await 确保 listener 已 drop (端口真正释放);
        // JoinHandle 不会 self-drop 产生问题 (abort 后 await 必然立即返回)
        drop((&mut instance.task).await);
    }
    info!("file-server (embedded) stopped, port released");
    Ok(())
}
