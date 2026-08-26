//! UserApp 开发资源回收（[`shared_types::UserappDevCleanup`] 契约实现）。
//!
//! app 删除（purge）时回收 UserAppBuilder 开发资源——per-app 开发容器 +
//! per-app RWO PVC +（Docker 模式）开发卷宿主目录（挂载压平四目录 + 旧布局遗留）。
//! 经契约注入 app_manager（其 runtime 视图无 agent 能力）；幂等，失败 best-effort
//! 由调用方决定（purge 路径 warn 不阻断，下次收敛）。

use std::sync::Arc;

use shared_types::ServiceType;
use tracing::info;

/// [`shared_types::UserappDevCleanup`] 实现：app 删除（purge）时回收 UserAppBuilder
/// 开发资源——per-app 开发容器 + per-app RWO PVC +（Docker 模式）宿主 bind 目录。
///
/// 经契约注入 app_manager（其 runtime 视图无 agent 能力）；幂等，失败 best-effort
/// 由调用方决定（purge 路径 warn 不阻断，下次收敛）。
pub struct UserappDevResourcesCleanup {
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
    /// 宿主注册表（purge 后同步摘除，防下一个请求 ensure 复活已删 app 的容器+PVC）。
    projects: Arc<crate::storage::ProjectStoreBackend>,
}

impl UserappDevResourcesCleanup {
    pub fn new(
        runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
        projects: Arc<crate::storage::ProjectStoreBackend>,
    ) -> Self {
        Self { runtime, projects }
    }
}

#[async_trait::async_trait]
impl shared_types::UserappDevCleanup for UserappDevResourcesCleanup {
    async fn cleanup(&self, app_id: &str) -> Result<(), String> {
        // 1. 停/删开发容器（幂等；失败不阻断 PVC 回收，K8s 下 PVC 有 Pod 引用时
        //    pvc-protection 会挂住删除——容器先删是 PVC 能删的前提）
        if let Err(e) = self
            .runtime
            .stop_container_by_identifier(app_id, &ServiceType::UserAppBuilder)
            .await
        {
            tracing::warn!(
                "[USERAPP_DEV_CLEANUP] stop dev container failed (continuing): app_id={app_id}: {e}"
            );
        } else {
            info!("[USERAPP_DEV_CLEANUP] dev container stopped: app_id={app_id}");
        }

        // 2. per-app 开发 PVC 回收（幂等；Docker 模式 trait no-op）
        self.runtime
            .destroy_workspace_pvc(app_id, &ServiceType::UserAppBuilder)
            .await
            .map_err(|e| format!("destroy dev PVC failed: {e}"))?;

        // 3. Docker 模式开发卷目录清理：经 **rcoder 容器内锚点路径**删除（bind 双向
        //    同步宿主）。不 resolve 宿主绝对路径——它在 rcoder 容器内不可见（OrbStack
        //    下是 VM fs，非宿主共享），历史实现因此静默失效（宿主 userapp-workspace
        //    残留 app-* 目录的根因）。K8s 模式锚点无 bind，is_dir false 自然跳过。
        //    新布局 dev/{user_id}/ 下四目录 + 旧布局 {锚点}/{app_id} 硬切遗留；
        //    user_id 不经 trait 契约传递（仅 app_id），按 app_id 唯一性通配扫
        //    dev/*/ 一层定位属主目录（per-user 目录数小，遍历成本可忽略）。
        {
            // 四段后缀 = 挂载压平布局的 app 侧段（单一事实源
            // paths::userapp_dev_app_suffixes；uid 不经 trait 契约传递，通配扫
            // dev/*/ 一层定位属主目录后拼后缀）
            let sub_paths = shared_types::paths::userapp_dev_app_suffixes(app_id);
            let dev_root = std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
                .join("dev");
            if dev_root.is_dir() {
                let mut rd = tokio::fs::read_dir(&dev_root)
                    .await
                    .map_err(|e| format!("read dev root {}: {e}", dev_root.display()))?;
                while let Some(user_dir) = rd
                    .next_entry()
                    .await
                    .map_err(|e| format!("iterate dev root: {e}"))?
                {
                    for sub in &sub_paths {
                        let target = user_dir.path().join(sub);
                        if target.exists() {
                            tokio::fs::remove_dir_all(&target).await.map_err(|e| {
                                format!("remove dev bind dir {}: {e}", target.display())
                            })?;
                            info!(
                                "[USERAPP_DEV_CLEANUP] dev bind dir removed: {}",
                                target.display()
                            );
                        }
                    }
                }
            }
            // 旧布局（{app_id} 直挂锚点根下）硬切遗留清理
            let legacy_dir =
                std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
                    .join(app_id);
            if legacy_dir.exists() {
                tokio::fs::remove_dir_all(&legacy_dir)
                    .await
                    .map_err(|e| format!("remove legacy dev dir {}: {e}", legacy_dir.display()))?;
                info!(
                    "[USERAPP_DEV_CLEANUP] legacy dev dir removed: {}",
                    legacy_dir.display()
                );
            }
        }

        // 4. 摘注册与探活缓存：purge 后注册表残留死 IP 会让下一个请求
        //    ensure→探活失败→重建（已删 app 的容器+PVC 复活）。
        //    durable：purge 期间并发 dev chat 的 durable insert 与本删除同走
        //    同步事务（消除"remove 入队→durable 提交→writer 重放删行"倒挂）
        if self.projects.remove_durable(app_id).await.is_some() {
            info!("[USERAPP_DEV_CLEANUP] dev registry entry removed: app_id={app_id}");
        }
        crate::userapp_forward::invalidate_probe_cache(app_id);

        info!("[USERAPP_DEV_CLEANUP] dev resources cleaned: app_id={app_id}");
        Ok(())
    }
}
