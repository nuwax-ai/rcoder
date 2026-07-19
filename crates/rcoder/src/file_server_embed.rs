//! 阶段2 方案C: rcoder 同进程嵌入 file-server。
//!
//! [`spawn_embedded_file_server`] 在 rcoder 进程内启动 file-server axum (端口 60000),
//! 经 [`SubvolumeWorkspaceResolver`] + 本模块 [`ContainerRuntimePathResolver`]
//! (包 `Arc<dyn ContainerRuntime>::resolve_workspace_path`) 解析 per-agent CephFS
//! subvolume 聚合路径。file-server 不加 kube 依赖, K8s 能力全经 rcoder ContainerRuntime。
//!
//! env 开关 `RCODER_EMBED_FILE_SERVER=true|1` 启用 (灰度); 配套 start-services.sh
//! 须检查本 env, 嵌入时不再单独启 file-server 二进制 (避免端口冲突)。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use container_runtime_api::ContainerRuntime;
use file_server::error::AppResult;
use file_server::{
    Config, FileServer, SubvolumeWorkspaceResolver, WorkspacePathResolver, WorkspaceResolver,
};
use shared_types::ServiceType;
use tracing::{info, warn};

/// 包 `Arc<dyn ContainerRuntime>` 实现 file-server 的 [`WorkspacePathResolver`] 窄 trait。
///
/// `resolve_workspace_path` 失败 (K8s API 抖动 / PVC 未 Bound / Docker 模式) → 返回 `None`
/// → [`SubvolumeWorkspaceResolver`] 降级到 LocalWorkspaceResolver (fail-open, 不阻断服务)。
pub(crate) struct ContainerRuntimePathResolver {
    runtime: Arc<dyn ContainerRuntime>,
}

impl ContainerRuntimePathResolver {
    pub(crate) fn new(runtime: Arc<dyn ContainerRuntime>) -> Self {
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
        // 2. PVC 不存在 → ensure + resolve + 迁移
        self.runtime
            .ensure_workspace(identifier, service_type)
            .await
            .map_err(|e| AppError::system(format!("ensure_workspace: {e}")))?;
        let base = match self.resolve(identifier, service_type).await? {
            Some(b) => b,
            None => return Ok(None), // ensure 后仍 None (异常), 让上层 fallback
        };
        run_lazy_migrate(&self.runtime, identifier, service_type).await;
        Ok(Some(base))
    }
}

/// 按 service_type 算 lazy_migrate 参数并执行 (async)。
async fn run_lazy_migrate(
    runtime: &Arc<dyn ContainerRuntime>,
    identifier: &str,
    service_type: &ServiceType,
) {
    let (pvc_env, subpath, dst_at_root) = match service_type {
        ServiceType::WebAgentRunner => ("RCODER_WORKSPACE_PVC_NAME", vec!["workspace"], false),
        ServiceType::ComputerAgentRunner => ("RCODER_COMPUTER_WORKSPACE_PVC_NAME", vec![], true),
        _ => return, // UserApp 不经 file-server (app_manager 直管)
    };
    crate::workspace_migrate::lazy_migrate(
        runtime, pvc_env, &subpath, identifier, service_type, identifier, dst_at_root,
    )
    .await;
}

/// 启动嵌入式 file-server (rcoder 同进程, 方案C)。
///
/// 构造 [`SubvolumeWorkspaceResolver`] (包 ContainerRuntime) + [`Config::load`] +
/// `tokio::spawn` serve (端口 60000)。任何阶段失败只 warn, 不阻断 rcoder 启动
/// (file-server 降级, rcoder 主服务不受影响)。
pub(crate) async fn spawn_embedded_file_server(runtime: Arc<dyn ContainerRuntime>) {
    let path_resolver = Arc::new(ContainerRuntimePathResolver::new(runtime));
    let fs_resolver: Arc<dyn WorkspaceResolver> =
        Arc::new(SubvolumeWorkspaceResolver::new(path_resolver));

    let fs_config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            warn!("load file-server config failed, embedded file-server not started: {e:#}");
            return;
        }
    };
    let address = format!("{}:{}", fs_config.listen_host, fs_config.port);
    let fs_server =
        match FileServer::builder(fs_config).with_workspace_resolver(fs_resolver).build() {
            Ok(s) => s,
            Err(e) => {
                warn!("build embedded file-server failed, not started: {e:#}");
                return;
            }
        };
    info!("file-server (embedded) starting on {}", address);
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(&address).await {
            Ok(listener) => {
                info!("file-server (embedded) listening on {}", address);
                if let Err(e) = fs_server.serve(listener).await {
                    warn!("embedded file-server serve exited: {e:#}");
                }
            }
            Err(e) => warn!("file-server bind {} failed: {e}", address),
        }
    });
}
