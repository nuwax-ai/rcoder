//! file-server 路由合并进 rcoder 主服务（同进程同端口，无独立 listener）。
//!
//! [`merged_router`] 构造 file-server 的基础路由（[`file_server::routes::api_router_base`]，
//! 排除 `/`、`/health`、`/api/v1/userapp` 与 swagger UI），由 `create_router` merge 进主
//! Router——老业务路径（/api/project、/api/computer、/api/git、/api/build）在主端口即可用；
//! userApp 域由 rcoder 侧转发层接管（透传到 per-app 开发容器内的 file-server）。
//!
//! 路由经 [`SubvolumeWorkspaceResolver`] + 本模块 [`ContainerRuntimePathResolver`]
//! （包 `Arc<dyn ContainerRuntime>::resolve_workspace_path`）解析 per-agent CephFS
//! subvolume 聚合路径。file-server 不加 kube 依赖，K8s 能力全经 rcoder ContainerRuntime。
//!
//! 历史：阶段2 方案C 曾为独立 60000 listener（`RCODER_EMBED_FILE_SERVER` 灰度 +
//! 运行时启停 admin API）；60000 端口现让位给反向代理（分流 TS/Rust），admin API
//! 与 CLI 子命令已删除。`RCODER_EMBED_FILE_SERVER` 仅剩 agent_runner 进程消费。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use axum::Router;
use container_runtime_api::WorkspaceRuntime;
use file_server::error::AppResult;
use file_server::{
    Config, FileServer, SubvolumeWorkspaceResolver, WorkspacePathResolver, WorkspaceResolver,
};
use shared_types::ServiceType;
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

/// ContainerRuntime 全局注册 (main 无条件调用)。
static RUNTIME: OnceLock<Arc<dyn WorkspaceRuntime>> = OnceLock::new();

/// 注册 ContainerRuntime (幂等, 首次生效; 重复注册保留首个)。
pub fn register_runtime(runtime: Arc<dyn WorkspaceRuntime>) {
    if RUNTIME.set(runtime).is_err() {
        tracing::debug!("workspace runtime already registered, keep first");
    }
}

/// 构造合并进 rcoder 主 Router 的 file-server 基础路由（无独立 listener/端口）。
///
/// 返回 `Err` 时主服务照常启动（缺 file-server 路由不致命，warn 可见）。
pub fn merged_router() -> Result<Router, String> {
    let Some(runtime) = RUNTIME.get().cloned() else {
        return Err(
            "workspace runtime not registered (file-server routes not mounted)".to_string(),
        );
    };

    let path_resolver = Arc::new(ContainerRuntimePathResolver::new(runtime));
    let fs_resolver: Arc<dyn WorkspaceResolver> =
        Arc::new(SubvolumeWorkspaceResolver::new(path_resolver));

    let fs_config = Config::load().map_err(|e| format!("load file-server config: {e:#}"))?;
    let fs_server = FileServer::builder(fs_config)
        .with_workspace_resolver(fs_resolver)
        .build()
        .map_err(|e| format!("build merged file-server: {e:#}"))?;
    fs_server
        .router_base()
        .map_err(|e| format!("build merged file-server router: {e:#}"))
}
