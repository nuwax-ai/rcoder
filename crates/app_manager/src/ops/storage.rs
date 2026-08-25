//! 持久存储管理（query/delete storage + orphan 检测）
//!
//! RBD 卷形态（rcoder 零挂载）：`exists` = PVC 归属（K8s label 集）/目录存在
//! （Docker）；`path` = PVC 名（K8s，非可挂载路径）/bind 源目录（Docker）；
//! `modified_at` 仅 Docker 可得（K8s 需容器运行，降级 None）。
//! `clear`：K8s = 删 PVC（数据清空语义等价——RBD 不可挂载无法逐文件清，
//! 卷在下次 create 自动重建）；Docker = 清目录内容。

use tracing::{info, warn};

use shared_types::ServiceType;

use crate::models::*;
use crate::utils::*;

impl crate::service::AppService {
    // ===== 持久存储管理（v2 §5.4）=====
    // 删应用默认保留数据；这组接口让 Java 显式管理残留存储。
    // StorageInfo 不含 size_bytes——需容器运行时 exec du，跨面语义不稳（见设计文档 §5.4）。

    /// 查询单个应用的持久存储状态。
    pub async fn get_app_storage(&self, app_id: &str) -> AppResult<StorageInfo> {
        validate_app_id(app_id)?;
        let is_orphan = self.is_storage_orphan(app_id).await;
        let (exists, path) = self.storage_path_info(app_id).await?;
        let modified_at = if shared_types::is_kubernetes_runtime() {
            None // RBD 卷不可挂载，无路径视角；容器 exec stat 属运行时依赖，降级
        } else {
            tokio::fs::metadata(&path)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
        };
        Ok(StorageInfo {
            app_id: app_id.to_string(),
            exists,
            path: path.to_string_lossy().to_string(),
            modified_at,
            is_orphan,
        })
    }

    /// 存储标识 + 存在性（K8s：PVC 名 + label 集含否；Docker：bind 目录 + 本地 stat）。
    async fn storage_path_info(&self, app_id: &str) -> AppResult<(bool, std::path::PathBuf)> {
        let path = self
            .runtime
            .workspace_volume_name(app_id, &ServiceType::UserApp)
            .await
            .map_err(|e| map_runtime_error("[APP] workspace_volume_name failed", e))?;
        let path = std::path::PathBuf::from(path);
        if shared_types::is_kubernetes_runtime() {
            // PVC 归属 = label 集（list_workspace_identifiers 枚举 per-app PVC，
            // 含已 delete 的孤儿）；path 字段是 PVC 名而非可挂载路径
            let exists = self
                .runtime
                .list_workspace_identifiers(&ServiceType::UserApp)
                .await
                .map_err(|e| map_runtime_error("[APP] list_workspace_identifiers failed", e))?
                .contains(&app_id.to_string());
            Ok((exists, path))
        } else {
            let exists = tokio::fs::metadata(&path).await.is_ok();
            Ok((exists, path))
        }
    }

    /// 校验 app 计算资源已不存在（clear/destroy 共用前置：必须先 delete app，否则 INVALID_STATE）。
    /// `op` 用于错误描述（如 "clearing storage" / "destroying PVC"）。
    async fn ensure_app_deleted(&self, app_id: &str, op: &str) -> AppResult<()> {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(_)) => Err(AppOperationError::InvalidState(format!(
                "app {app_id} still exists, delete it before {op}"
            ))),
            Ok(None) => Ok(()),
            Err(e) => {
                warn!("[APP] query app status failed app_id={}: {}", app_id, e);
                Err(AppOperationError::Backend(format!(
                    "failed to query app status: {e}"
                )))
            }
        }
    }

    /// per-app 数据卷的 rcoder 容器内锚点路径（`{锚点}/prod/{user_id}/data/{app_id}`，
    /// bind 双向同步宿主——Docker 模式 clear/destroy 的数据目录定位）。
    /// owner user_id 查元数据，缺失/空白兜底 app_id（与 docker_app_runtime 组装
    /// bind 源的兜底一致）。
    fn app_data_dir(&self, app_id: &str) -> std::path::PathBuf {
        let uid = self
            .metadata
            .lookup(app_id)
            .and_then(|r| r.user_id)
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| app_id.to_string());
        std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
            .join(shared_types::paths::userapp_prod_data_subpath(&uid, app_id))
    }

    /// 清空应用持久存储内容（数据语义；卷对象去留按运行时能力）。
    /// 安全约束：仅当 app 计算资源已不存在时允许（否则 INVALID_STATE）。
    /// - K8s/RBD：rcoder 不可挂载无法逐文件清 → 删 PVC（下次 create 自动重建空卷，
    ///   数据清空语义等价；handbook 注明）。
    /// - Docker：清 bind 目录内容（留目录本身，可恢复）。
    pub async fn clear_app_storage(&self, app_id: &str) -> AppResult<()> {
        validate_app_id(app_id)?;
        self.ensure_app_deleted(app_id, "clearing storage").await?;
        if shared_types::is_kubernetes_runtime() {
            self.runtime
                .destroy_app_pvc(app_id)
                .await
                .map_err(|e| map_runtime_error("destroy_app_pvc (clear storage) failed", e))?;
            info!(
                "[APP] app storage cleared (PVC removed, recreated empty on next create): {}",
                app_id
            );
            return Ok(());
        }
        let app_dir = self.get_host_app_dir(app_id);
        if app_dir.exists()
            && let Err(e) = Self::purge_dir_contents(&app_dir).await
        {
            return Err(map_io_error("failed to clear storage", e, false));
        }
        // per-app 数据卷目录（prod/{user_id}/data/{app_id}——PG/dbx 持久数据）：
        // "清空持久数据"语义的主体。K8s 分支 destroy_app_pvc 已删 data PVC（上 方
        // return），此处仅 Docker。owner 查元数据，缺失兜底 app_id（与 bind 源
        // 组装的兜底一致；元数据行在 clear 场景恒保留）。
        let data_dir = self.app_data_dir(app_id);
        if data_dir.exists()
            && let Err(e) = Self::purge_dir_contents(&data_dir).await
        {
            return Err(map_io_error("failed to clear app data", e, false));
        }
        info!("[APP] app storage cleared: {}", app_id);
        Ok(())
    }

    /// 销毁应用持久存储 PVC（高危·不可逆·释放配额）。
    ///
    /// 安全约束：① 仅当 app 计算资源已不存在（已 delete）时允许（否则 INVALID_STATE）；
    /// ② body `confirm` 必须等于 app_id（否则 VALIDATION，防误调/防脚本批量误删）。
    /// K8s：删 PVC 对象。Docker：无 PVC，等价删 bind 目录。
    /// 幂等：PVC 已不存在也返回成功。
    pub async fn destroy_app_storage(&self, app_id: &str, confirm: &str) -> AppResult<()> {
        self.destroy_app_storage_keep_metadata(app_id, confirm)
            .await?;
        // 独立 storage/destroy 接口：与 PVC 同生命周期，元数据行同步删除
        // （三档删除语义的第三档）。
        self.metadata.record_deleted(app_id).await;
        info!("[APP] app PVC destroyed: {}", app_id);
        Ok(())
    }

    /// 销毁持久存储但**保留业务元数据行**（delete_app 的 purge 分支专用）。
    ///
    /// 三档删除语义：delete/purge 保留行（误删找回——重建同 ID 应用后 name/created_at
    /// 仍在），仅独立的 storage/destroy 接口（[`Self::destroy_app_storage`]）删行。
    /// 此前 purge 直接复用 destroy_app_storage 把行也删了，违反契约。
    pub(crate) async fn destroy_app_storage_keep_metadata(
        &self,
        app_id: &str,
        confirm: &str,
    ) -> AppResult<()> {
        validate_app_id(app_id)?;
        if confirm != app_id {
            return Err(AppOperationError::Validation(format!(
                "confirm must equal app_id for destroy (high-risk op): got confirm='{confirm}'"
            )));
        }
        self.ensure_app_deleted(app_id, "destroying PVC").await?;
        self.runtime
            .destroy_app_pvc(app_id)
            .await
            .map_err(|e| map_runtime_error("destroy_app_pvc failed", e))?;
        // Docker 模式追加：per-app 数据卷 bind 目录（{userapp 锚点}/prod/{user_id}/
        // data/{app_id}）——对应 K8s destroy_app_pvc 删 `-data` PVC（已含，此分支
        // 不执行）。硬错不吞：destroy 是显式高危操作（confirm=app_id），残留即孤儿；
        // 失败时外层 record_deleted 未执行，幂等重试收敛。user_id 查元数据（行此时尚
        // 未删），缺失兜底 app_id（与 bind 源组装的兜底一致）。
        if !shared_types::is_kubernetes_runtime() {
            let data_dir = self.app_data_dir(app_id);
            if data_dir.exists() {
                tokio::fs::remove_dir_all(&data_dir)
                    .await
                    .map_err(|e| map_io_error("destroy app data dir failed", e, false))?;
                info!("[APP] app data dir destroyed: {}", data_dir.display());
            }
        }
        // UserApp 开发资源回收（UserAppBuilder 开发容器 + per-app 开发 PVC）：
        // 经 UserappDevCleanup 契约回调宿主（app_manager 的 runtime 视图无 agent
        // 能力，ISP 分层）；best-effort——失败仅 warn 不阻断 purge，下次幂等收敛。
        let dev_cleanup = self.dev_cleanup.read().expect("dev_cleanup lock").clone();
        match dev_cleanup {
            Some(cleanup) => {
                if let Err(e) = cleanup.cleanup(app_id).await {
                    warn!(
                        "[APP] userapp dev resources cleanup failed (best-effort, will converge on next purge): app_id={app_id}: {e}"
                    );
                } else {
                    info!("[APP] userapp dev resources cleaned: app_id={app_id}");
                }
            }
            None => {
                warn!(
                    "[APP] dev cleanup not injected, skip UserAppBuilder resources recycle: app_id={app_id}"
                );
            }
        }
        info!("[APP] app PVC destroyed (metadata retained): {}", app_id);
        Ok(())
    }

    /// 清空目录内容 (逐子项 remove), 保留目录本身（Docker 模式专用）。
    pub(super) async fn purge_dir_contents(dir: &std::path::Path) -> std::io::Result<()> {
        let mut rd = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let p = entry.path();
            if p.is_dir() {
                tokio::fs::remove_dir_all(&p).await?;
            } else {
                tokio::fs::remove_file(&p).await?;
            }
        }
        Ok(())
    }

    /// 分页查询持久存储（强制分页，无全量模式）。
    /// 过滤：orphan_only、app_ids 生效；tenant_id/space_id 在无状态下不支持（rcoder 不持
    /// app→租户映射），提供则 warn 忽略。
    pub async fn query_storage(
        &self,
        request: QueryStorageRequest,
    ) -> AppResult<PaginatedResponse<StorageInfo>> {
        if request.page == 0 {
            return Err(AppOperationError::Validation(
                "page starts from 1".to_string(),
            ));
        }
        if request.page_size == 0 || request.page_size > 100 {
            return Err(AppOperationError::Validation(
                "page_size must be in 1..=100".to_string(),
            ));
        }
        let filters = request.filters.unwrap_or_default();
        if filters.tenant_id.is_some() || filters.space_id.is_some() {
            warn!(
                "[APP] query_storage tenant_id/space_id filters not supported in stateless mode (rcoder holds no app→tenant mapping), ignored"
            );
        }
        // 现有 app 集合（供 is_orphan），一次 list 调用
        let existing: std::collections::HashSet<String> = self
            .runtime
            .list_deployments()
            .await
            .map_err(|e| map_runtime_error("[APP] list_deployments failed", e))?
            .into_iter()
            .map(|s| s.app_id)
            .collect();
        // 候选 = 所有"有持久数据"的 app：枚举 UserApp 的 per-app PVC（含**已 delete 但
        // PVC 保留的孤儿**）——这才是 orphan 检测的数据源。再并入运行中的 app（existing，
        // 兜底；正常 running app 都有 PVC，已含）。
        let mut entries: std::collections::HashSet<String> = self
            .runtime
            .list_workspace_identifiers(&ServiceType::UserApp)
            .await
            .map_err(|e| map_runtime_error("[APP] list_workspace_identifiers failed", e))?
            .into_iter()
            .collect();
        for id in existing.iter() {
            entries.insert(id.clone());
        }
        let mut entries: Vec<String> = entries.into_iter().collect();
        entries.sort();
        let app_ids_filter = filters.app_ids.as_deref();
        let filtered: Vec<String> = entries
            .into_iter()
            .filter(|app_id| {
                if let Some(ids) = app_ids_filter
                    && !ids.iter().any(|x| x == app_id)
                {
                    return false;
                }
                if filters.orphan_only.unwrap_or(false) && existing.contains(app_id) {
                    return false;
                }
                true
            })
            .collect();
        let total = filtered.len() as u64;
        let page = request.page as usize;
        let page_size = request.page_size as usize;
        let start = page.saturating_sub(1) * page_size;
        let paged: Vec<String> = filtered.into_iter().skip(start).take(page_size).collect();

        let mut items = Vec::with_capacity(paged.len());
        for app_id in paged {
            let is_orphan = !existing.contains(&app_id);
            // 单项标识解析失败（瞬时 K8s API 抖动）不中断整个列表：warn + 标记 not exist
            let (exists, path) = match self.storage_path_info(&app_id).await {
                Ok(ok) => ok,
                Err(e) => {
                    warn!(
                        "[APP] list storage resolve {} failed, mark not exist: {}",
                        app_id, e
                    );
                    (false, std::path::PathBuf::new())
                }
            };
            let modified_at = if shared_types::is_kubernetes_runtime() {
                None
            } else {
                tokio::fs::metadata(&path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
            };
            items.push(StorageInfo {
                app_id,
                exists,
                path: path.to_string_lossy().to_string(),
                modified_at,
                is_orphan,
            });
        }
        let total_pages = if total == 0 {
            1
        } else {
            total.div_ceil(page_size as u64) as u32
        };
        Ok(PaginatedResponse {
            items,
            pagination: Pagination {
                page: request.page,
                page_size: request.page_size,
                total,
                total_pages,
            },
        })
    }

    /// 存储是否为孤儿（无对应运行应用）。Ok(None)=orphan；Ok(Some)/Err=非 orphan（保守）。
    pub(super) async fn is_storage_orphan(&self, app_id: &str) -> bool {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(e) => {
                // 瞬时 API 错误保守视为"非 orphan"（避免误删在用数据），但落日志可见
                warn!(
                    "[APP] is_storage_orphan query status failed app_id={}: {}",
                    app_id, e
                );
                false
            }
        }
    }
}
