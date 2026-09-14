//! Custom Page 预览协调器装配（rcoder 主进程；config 段 `preview_coordinator`）。
//!
//! 装配链：存储（K8s=平台 PG / 其他=进程内）+ 宿主证据（kube / 单机）+ 内部
//! 令牌 → `PreviewCoordinator`（执行器由 file_server_embed 的 merged_router 注入
//! DevServerManager 适配）。enabled 且令牌缺失 → 启动 fail-fast（不引入弱令牌）。
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use shared_types::PreviewLifecycleStore;
use tracing::info;

/// 装配产物：merged_router 与后台任务/内部端点所需的协调器句柄。
pub struct PreviewAssembly {
    pub store: Option<Arc<dyn PreviewLifecycleStore>>,
    pub config: Option<preview_coordinator::CoordinatorConfig>,
    pub evidence: Arc<dyn preview_coordinator::HostEvidence>,
    pub token: String,
}

impl PreviewAssembly {
    pub fn enabled(&self) -> bool {
        self.config.as_ref().is_some_and(|c| c.enabled) && self.store.is_some()
    }
}

/// 构建存储与证据（不构造协调器——执行器在 merged_router 内注入）。
pub async fn bootstrap(config: &crate::config::AppConfig) -> Result<PreviewAssembly> {
    let section = config.preview_coordinator.clone();
    let Some(section) = section.filter(|c| c.enabled) else {
        return Ok(PreviewAssembly {
            store: None,
            config: None,
            evidence: Arc::new(preview_coordinator::SingleInstanceEvidence),
            token: String::new(),
        });
    };

    let token = preview_coordinator::internal_token_from_env_named(&section.internal_token_env)
        .context("preview_coordinator.enabled 但内部令牌缺失")?;
    if token.len() < 16 {
        bail!(
            "preview internal token too short (min 16 chars, env {})",
            section.internal_token_env
        );
    }

    let store = build_store(config).await?;
    let evidence = build_evidence().await;
    info!(
        backend = %store_kind(),
        "preview coordinator enabled (dev lifecycle coordinated)"
    );
    Ok(PreviewAssembly {
        store: Some(store),
        config: Some(section),
        evidence,
        token,
    })
}

fn store_kind() -> &'static str {
    if cfg!(feature = "kubernetes") {
        "postgres"
    } else {
        "in-process"
    }
}

#[cfg_attr(not(feature = "kubernetes"), expect(unused_variables))]
async fn build_store(config: &crate::config::AppConfig) -> Result<Arc<dyn PreviewLifecycleStore>> {
    #[cfg(feature = "kubernetes")]
    {
        // 证据面 PG：userapp_storage.postgres 优先（同库同实例），回落主 storage 段。
        let main_pg = (config.storage.backend == crate::config::StorageBackend::Postgres)
            .then(|| config.storage.postgres.clone());
        let pg = config.userapp_storage.postgres.clone().or(main_pg);
        let pg = pg.with_context(|| {
            "preview coordinator (kubernetes build) requires postgres config \
             ([userapp_storage].postgres or [storage].postgres)"
        })?;
        let store = rcoder_storage::preview_lifecycle::PgPreviewStore::connect(&pg)
            .await
            .context("initialize preview PostgreSQL storage")?;
        Ok(Arc::new(store))
    }
    #[cfg(not(feature = "kubernetes"))]
    {
        // Compose/本地单节点：进程内权威存储（单进程 CAS 天然成立，无跨进程读者）。
        Ok(Arc::new(preview_coordinator::InProcessPreviewStore::new()))
    }
}

async fn build_evidence() -> Arc<dyn preview_coordinator::HostEvidence> {
    #[cfg(feature = "kubernetes")]
    {
        Arc::new(KubeHostEvidence::from_env().await)
    }
    #[cfg(not(feature = "kubernetes"))]
    {
        Arc::new(preview_coordinator::SingleInstanceEvidence)
    }
}

/// K8s 宿主证据：列本 namespace 的 rcoder 主 Pod，比对 UID。
/// **fail-closed**：kube API 出错时按"宿主仍存在"处理（拒绝接管）——
/// 恢复证据必须是"不存在"的积极证明，错误不构成证据。
#[cfg(feature = "kubernetes")]
struct KubeHostEvidence {
    client: Option<kube::Client>,
    namespace: String,
}

#[cfg(feature = "kubernetes")]
impl KubeHostEvidence {
    async fn from_env() -> Self {
        let namespace =
            std::env::var("RCODER_K8S_NAMESPACE").unwrap_or_else(|_| "default".to_string());
        let client = kube::Client::try_default().await.ok();
        if client.is_none() {
            tracing::warn!(
                "preview KubeHostEvidence: kube client unavailable; takeover stays refused (fail-closed)"
            );
        }
        Self { client, namespace }
    }
}

#[cfg(feature = "kubernetes")]
#[async_trait::async_trait]
impl preview_coordinator::HostEvidence for KubeHostEvidence {
    async fn host_pod_exists(&self, pod_uid: &str) -> bool {
        use kube::api::{Api, ListParams};
        let Some(client) = self.client.clone() else {
            return true; // fail-closed：无法证明宿主消失 → 视为存在
        };
        let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client, &self.namespace);
        let selector = ListParams::default().labels("app.kubernetes.io/component=rcoder-main");
        match pods.list(&selector).await {
            Ok(list) => list
                .items
                .iter()
                .any(|pod| pod.metadata.uid.as_deref() == Some(pod_uid)),
            Err(error) => {
                tracing::warn!(
                    pod_uid,
                    "preview host evidence lookup failed (fail-closed): {error}"
                );
                true
            }
        }
    }
}
