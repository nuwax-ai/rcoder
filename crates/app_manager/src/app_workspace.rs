//! UserApp workspace 目录 + PVC 就绪（从 service.rs 拆出，extension-impl）。
//!
//! ensure_app_workspace_ready / get_container_app_dir / get_host_app_dir / create_app_dirs。

use std::path::PathBuf;

use tokio::fs;

use shared_types::ServiceType;

use super::models::*;
use super::service::AppService;
use super::utils::*;

impl AppService {
    /// ensure UserApp per-app 工作空间就绪 (K8s): ensure PVC 带 requests.storage 用户配额 + 重试
    /// resolve 等 ceph-csi provision subvolumePath (SC Immediate 后秒级, 慢可达 10s+)。
    ///
    /// 必须在 create_app_dirs (建目录) + create_deployment (Docker bind mount 需源目录存在) 之前调用:
    /// - K8s: ensure PVC 带配额 + 等 subvolumePath → 后续 create_app_dirs resolve per-app 成功,
    ///   建 code/data/logs 在 per-app PVC 根 (app pod 挂同 PVC, 无分裂); create_deployment 命中
    ///   PVC active 复用 (配额不丢, 因首次 ensure 已带配额)。
    /// - Docker: 无 per-app PVC → no-op (create_app_dirs 走共享 Local, create_deployment bind mount)。
    pub(crate) async fn ensure_app_workspace_ready(
        &self,
        app_id: &str,
        storage_size: Option<&str>,
    ) -> AppResult<()> {
        if !shared_types::is_kubernetes_runtime() {
            return Ok(()); // Docker 无 per-app PVC
        }
        self.runtime
            .ensure_workspace(app_id, &ServiceType::UserApp, storage_size)
            .await
            .map_err(|e| {
                AppOperationError::Backend(format!("ensure UserApp PVC (app_id={app_id}): {e}"))
            })?;
        // 重试 resolve 等 ceph-csi provision subvolumePath 填充 (PVC Bound 后 PV subvolumePath 仍有延迟)
        const MAX_RETRIES: u32 = 15;
        let mut attempt: u32 = 0;
        loop {
            match self
                .runtime
                .resolve_workspace_path(app_id, &ServiceType::UserApp)
                .await
            {
                Ok(Some(_)) | Ok(None) => return Ok(()),
                Err(e) => {
                    attempt += 1;
                    if attempt < MAX_RETRIES {
                        tracing::debug!(
                            "[APP] UserApp PVC subvolumePath pending ({}/{}, app_id={}): {}",
                            attempt,
                            MAX_RETRIES,
                            app_id,
                            e
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    } else {
                        return Err(AppOperationError::Backend(format!(
                            "UserApp PVC subvolumePath 未就绪 (app_id={app_id}, 重试 {MAX_RETRIES} 次): {e}"
                        )));
                    }
                }
            }
        }
    }

    /// 获取应用目录的宿主机路径（Docker bind mount 源）
    ///
    /// Docker 模式：rcoder 通常也运行在容器内，需经 HostPathResolver 将容器内路径
    /// 转为宿主机路径；解析失败回退到原路径。K8s 模式此值不被使用 (subPath)。
    ///
    /// 注意: get_container_app_dir 现返回 Result (K8s per-app 失败 Fail Fast)。本函数保持
    /// PathBuf 签名 (build_container_params 不感知错误), K8s 模式此值本就不用, resolve 失败时
    /// 降级共享路径即可; Docker 模式 resolve Ok(None) → 共享 (正常)。
    pub(crate) async fn get_host_app_dir(&self, app_id: &str) -> PathBuf {
        let p = self
            .get_container_app_dir(app_id)
            .await
            .unwrap_or_else(|_| PathBuf::from(self.config.get_workspace_root()).join(app_id));
        if let Some(resolver) = self.path_resolver.get("default") {
            resolver.resolve_to_host_path(&p).unwrap_or(p)
        } else {
            p
        }
    }

    /// 创建应用工作空间子目录（code/data/logs）。在 ensure_app_workspace_ready (K8s ensure PVC +
    /// 等 subvolumePath) 之后、create_deployment (Docker bind mount 需源目录存在) 之前调用。
    /// Docker: 共享 Local; K8s: per-app PVC 根 (ensure_app_workspace_ready 已确保 resolve 成功)。
    pub(crate) async fn create_app_dirs(&self, app_id: &str) -> AppResult<()> {
        let app_dir = self.get_container_app_dir(app_id).await?;
        fs::create_dir_all(app_dir.join("code"))
            .await
            .map_err(|e| map_io_error("failed to create code dir", e, false))?;
        fs::create_dir_all(app_dir.join("data"))
            .await
            .map_err(|e| map_io_error("failed to create data dir", e, false))?;
        fs::create_dir_all(app_dir.join("logs"))
            .await
            .map_err(|e| map_io_error("failed to create logs dir", e, false))?;
        Ok(())
    }

    /// 获取应用目录（rcoder 视角）。
    ///
    /// - K8s per-app: `resolve_workspace_path` 拿 per-app subvolume 聚合路径
    ///   (`{cephfs_root}/{subvolumePath}` = per-app PVC 根); UserApp pod 挂 per-app PVC 根到 /app
    ///   (subPath=None), 故 rcoder 写 PVC 根 (不 join app_id)。
    /// - Docker/无 Ceph: resolve 返回 None → 共享 `workspace_root/{app_id}` (= apps/{app_id},
    ///   运行时适配, 非 per-app 失败)。
    /// - K8s per-app resolve 失败 (Err): **Fail Fast** 返回 Backend 错误, 不 fallback 共享
    ///   (避免 per-app PVC + 共享 PVC 数据面分裂, 见 code-review M1/M2)。
    pub(super) async fn get_container_app_dir(&self, app_id: &str) -> AppResult<PathBuf> {
        match self
            .runtime
            .resolve_workspace_path(app_id, &ServiceType::UserApp)
            .await
        {
            Ok(Some(base)) => Ok(PathBuf::from(base)), // K8s per-app PVC 根 (不 join app_id)
            Ok(None) => Ok(PathBuf::from(self.config.get_workspace_root()).join(app_id)), // Docker 共享 Local
            Err(e) => Err(AppOperationError::Backend(format!(
                "UserApp per-app PVC resolve 失败 (app_id={app_id}): {e} — 检查 cephfs-root 挂载 + PVC Bound 状态"
            ))),
        }
    }
}
