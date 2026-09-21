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
/// 生命周期或 Userapp Deployment —— 类型声明即编译期约束。
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
        _ => return, // Userapp 不经 file-server (app_manager 直管)
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

/// merged_router 装配产物：file-server 路由 + 预览协调器句柄（None=未启用）。
///
/// 协调器在此构造的原因：执行器（DevServerExecutor）须持 DevServerManager，
/// 而 DevServerManager 与协调器一同回注 file-server AppState（打破构造环）。
pub struct MergedFileServer {
    pub router: Router,
    pub coordinator: Option<Arc<preview_coordinator::PreviewCoordinator>>,
}

/// 构造合并进 rcoder 主 Router 的 file-server 基础路由（无独立 listener/端口）。
///
/// `preview`（None 或 disabled）→ 不装配协调器，全部现状行为。
/// 返回 `Err` 时主服务照常启动（缺 file-server 路由不致命，warn 可见）。
pub fn merged_router(
    preview: crate::preview_assembly::PreviewAssembly,
) -> Result<MergedFileServer, String> {
    let Some(runtime) = RUNTIME.get().cloned() else {
        return Err(
            "workspace runtime not registered (file-server routes not mounted)".to_string(),
        );
    };

    let path_resolver = Arc::new(ContainerRuntimePathResolver::new(runtime));
    let fs_resolver: Arc<dyn WorkspaceResolver> =
        Arc::new(SubvolumeWorkspaceResolver::new(path_resolver));

    let fs_config = Config::load().map_err(|e| format!("load file-server config: {e:#}"))?;

    // 预览协调器装配（存储/证据/令牌已由 preview_assembly::bootstrap 构建校验）：
    // dev_manager → executor → coordinator，三者同实例回注 AppState。
    let mut coordinator: Option<Arc<preview_coordinator::PreviewCoordinator>> = None;
    let mut dev_manager_override: Option<Arc<file_server::DevServerManager>> = None;
    let mut preview_state: Option<Arc<dyn shared_types::PreviewCoordination>> = None;
    if preview.enabled() {
        let store = preview
            .store
            .clone()
            .ok_or_else(|| "preview assembly lacks store".to_string())?;
        let mut section = preview
            .config
            .clone()
            .ok_or_else(|| "preview assembly lacks config".to_string())?;
        // 对等副本主 API 端口：派发目标与本进程对外 API 同端口（config.port 未在此
        // 作用域，由调用方在 bootstrap 后覆写；此处兜底默认）。
        if section.peer_api_port == 0 {
            section.peer_api_port = 8086;
        }
        let dev_manager = Arc::new(file_server::DevServerManager::new(Arc::new(
            fs_config.clone(),
        )));
        let executor = Arc::new(file_server::service::dev_server::DevServerExecutor::new(
            dev_manager.clone(),
        ));
        let service = Arc::new(preview_coordinator::PreviewCoordinator::new(
            store,
            executor,
            preview.evidence.clone(),
            preview.token.clone(),
            section,
        ));
        coordinator = Some(service.clone());
        preview_state = Some(service);
        dev_manager_override = Some(dev_manager);
    }

    let mut builder = FileServer::builder(fs_config).with_workspace_resolver(fs_resolver);
    if let Some(manager) = dev_manager_override {
        builder = builder.with_dev_server(manager);
    }
    if let Some(state) = preview_state {
        builder = builder.with_preview_coordination(Some(state));
    }
    let fs_server = builder
        .build()
        .map_err(|e| format!("build merged file-server: {e:#}"))?;
    let router = fs_server
        .router_base()
        .map_err(|e| format!("build merged file-server router: {e:#}"))?;
    Ok(MergedFileServer {
        router,
        coordinator,
    })
}

/// 60000 分流代理的内嵌配置构造（main.rs 收口点；2026-09-19 事故治本）。
///
/// 两条路径（config.yml 有段 map 分支 / 无段 default 兜底分支）统一在此
/// 叠加 N07 受管声明 env `FILE_SERVER_PROXY_PUBLIC_BIND`（OR 语义：env 或
/// config.yml 任一声明即放行，词表与独立进程形态共用 `"1"`/`"true"`）。
/// 修复前内嵌形态只有 config.yml 一条声明通道——env 设了却没用。
pub fn embedded_proxy_config(
    section: Option<file_server_proxy::FileServerProxyConfig>,
    preview_enabled: bool,
    rcoder_port: u16,
) -> file_server_proxy::FileServerProxyConfig {
    let mut config = section
        .map(|mut c| {
            if preview_enabled {
                c.coordinated_dev_lifecycle = true;
            }
            c
        })
        .unwrap_or_else(|| file_server_proxy::FileServerProxyConfig {
            rust_upstream_port: rcoder_port,
            coordinated_dev_lifecycle: preview_enabled,
            ..file_server_proxy::FileServerProxyConfig::default()
        });
    config.apply_public_bind_env();
    config
}

#[cfg(test)]
mod embedded_config_tests {
    use super::*;

    /// env 变更测试串行锁（env 是进程全局——避免并行测试互踩）。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 测试专用 env 写入：Rust 2024 将 set_var/remove_var 标记 unsafe
    /// （进程全局可变状态）。仅在串行锁内使用——豁免通道仅限测试模块
    /// （workspace lint 注释明示；生产代码保持 deny unsafe）。
    #[allow(unsafe_code)]
    fn set_env(value: Option<&str>) {
        // SAFETY: ENV_LOCK 保证同一时刻仅一个测试线程操作该 env；
        // 被测构造函数在此期间同步读取，无并发读者窗口。
        match value {
            Some(v) => unsafe { std::env::set_var("FILE_SERVER_PROXY_PUBLIC_BIND", v) },
            None => unsafe { std::env::remove_var("FILE_SERVER_PROXY_PUBLIC_BIND") },
        }
    }

    /// 反例（修复前缺失）：内嵌构造路径 + env=1 → public_bind_declared==true。
    #[test]
    fn embedded_config_env_channel_declares_public_bind() {
        let guard = ENV_LOCK.lock().unwrap();
        set_env(Some("1"));
        let config = embedded_proxy_config(None, false, 8086);
        assert!(
            config.public_bind_declared,
            "env FILE_SERVER_PROXY_PUBLIC_BIND=1 必须对内嵌形态生效（修复前只有独立进程形态消费）"
        );
        drop(config);
        drop(guard);
    }

    /// env 未设 + config 无键 → 默认放行（N07 修订：受管容器为主流形态，
    /// 默认 true；09-19 前"默认严格+各处声明"的门只挡自己人）。
    #[test]
    fn embedded_config_defaults_to_managed_public_bind() {
        let guard = ENV_LOCK.lock().unwrap();
        set_env(None);
        let config = embedded_proxy_config(None, false, 8086);
        assert!(
            config.public_bind_declared,
            "无显式配置时默认受管放行（使用方收紧需显式 false）"
        );
        drop(config);
        drop(guard);
    }

    /// 显式收紧通道：config 段显式 public_bind_declared:false + env 未设
    /// → 保持 false（使用方的严格模式约束不被默认值翻转）。
    #[test]
    fn embedded_config_explicit_false_not_flipped_by_default() {
        let guard = ENV_LOCK.lock().unwrap();
        set_env(None);
        let section = file_server_proxy::FileServerProxyConfig {
            public_bind_declared: false,
            ..file_server_proxy::FileServerProxyConfig::default()
        };
        let config = embedded_proxy_config(Some(section), false, 8086);
        assert!(
            !config.public_bind_declared,
            "config 显式 false 是使用方收紧指令，默认值不得翻转"
        );
        drop(config);
        drop(guard);
    }

    /// config.yml 显式 true + env 未设 → true（OR 语义的 config 侧）。
    #[test]
    fn embedded_config_section_declaration_preserved_without_env() {
        let guard = ENV_LOCK.lock().unwrap();
        set_env(None);
        let section = file_server_proxy::FileServerProxyConfig {
            public_bind_declared: true,
            ..file_server_proxy::FileServerProxyConfig::default()
        };
        let config = embedded_proxy_config(Some(section), false, 8086);
        assert!(config.public_bind_declared);
        drop(config);
        drop(guard);
    }

    /// env 三态词表边界："true"/"1" → Some(true)；"0"/"false" → Some(false)
    /// （显式收紧通道）；空串/其它/未设 → None（用形态默认 true）。
    #[test]
    fn embedded_config_env_word_list_boundaries() {
        for (value, expect) in [
            ("1", Some(true)),
            ("true", Some(true)),
            ("TRUE", Some(true)),
            (" 1 ", Some(true)),
            ("0", Some(false)),
            ("false", Some(false)),
            ("FALSE", Some(false)),
            ("", None),
            ("yes", None),
        ] {
            assert_eq!(
                file_server_proxy::FileServerProxyConfig::env_public_bind_setting(Some(value)),
                expect,
                "value={value:?}"
            );
        }
        assert_eq!(
            file_server_proxy::FileServerProxyConfig::env_public_bind_setting(None),
            None
        );
    }

    /// env 显式收紧（优先级最高）：config 默认 true + env=false → false。
    #[test]
    fn embedded_config_env_false_overrides_default() {
        let guard = ENV_LOCK.lock().unwrap();
        set_env(Some("false"));
        let config = embedded_proxy_config(None, false, 8086);
        assert!(
            !config.public_bind_declared,
            "env 显式 false 是使用方收紧指令，优先于默认 true"
        );
        drop(config);
        drop(guard);
    }
}
