//! 持久存储管理（query/delete storage + orphan 检测）

use tracing::{info, warn};

use shared_types::ServiceType;

use super::models::*;
use super::utils::*;

impl super::service::AppService {
    // ===== 持久存储管理（v2 §5.4）=====
    // 删应用默认保留数据；这组接口让 Java 显式管理残留存储。
    // StorageInfo 不含 size_bytes——CephFS 上不能用 du（详见设计文档 §5.4）。

    /// 查询单个应用的持久存储状态（O(1) stat，不递归）。
    pub async fn get_app_storage(&self, app_id: &str) -> AppResult<StorageInfo> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let metadata = tokio::fs::metadata(&app_dir).await.ok();
        let exists = metadata.is_some();
        let modified_at = metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
        let is_orphan = self.is_storage_orphan(app_id).await;
        Ok(StorageInfo {
            app_id: app_id.to_string(),
            exists,
            path: app_dir.to_string_lossy().to_string(),
            modified_at,
            is_orphan,
        })
    }

    /// 清空应用的持久存储。安全约束：仅当 app 计算资源已不存在时允许（否则 INVALID_STATE）。
    pub async fn delete_app_storage(&self, app_id: &str) -> AppResult<()> {
        validate_app_id(app_id)?;
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(_)) => {
                return Err(AppOperationError::InvalidState(format!(
                    "app {app_id} still exists, delete it before clearing storage (to avoid corrupting in-use data)"
                )));
            }
            Ok(None) => {}
            Err(e) => {
                warn!("[APP] query app status failed app_id={}: {}", app_id, e);
                return Err(AppOperationError::Backend(format!(
                    "failed to query app status: {e}"
                )));
            }
        }
        let app_dir = self.get_container_app_dir(app_id).await?;
        // K8s per-agent: app_dir = per-app PVC 根 (ceph-csi subvol 根), 清空内容不删根
        // (删 subvol 根破坏 PV subvolumePath → pod 重启挂载异常)
        if app_dir.exists()
            && let Err(e) = Self::purge_dir_contents(&app_dir).await
        {
            return Err(map_io_error("failed to clear storage", e, false));
        }
        info!("[APP] app storage cleared: {}", app_id);
        Ok(())
    }

    /// 清空目录内容 (逐子项 remove), 保留目录本身。
    /// purge per-agent PVC 根 (ceph-csi subvol 根) 必须用此 —— `remove_dir_all` 删 subvol 根
    /// 会破坏 PV `csi.volumeAttributes.subvolumePath` (PVC 仍在但 subvol 路径不存在 → pod 重启挂载异常)。
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
        // 兜底；正常 running app 都有 PVC，已含）。list_deployments 只能拿运行中的，看不到
        // 孤儿 PVC，故旧实现（候选仅来自 existing）会让 storage/query 永远查不到孤儿。
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
            // resolve 失败 (K8s per-app PVC 未就绪) 不中断整个列表: warn + 标记 not exist
            let (exists, path, modified_at) = match self.get_container_app_dir(&app_id).await {
                Ok(app_dir) => {
                    let metadata = tokio::fs::metadata(&app_dir).await.ok();
                    let modified_at = metadata
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
                    (metadata.is_some(), app_dir.to_string_lossy().to_string(), modified_at)
                }
                Err(e) => {
                    warn!(
                        "[APP] list storage resolve {} failed, mark not exist: {}",
                        app_id,
                        e
                    );
                    (false, String::new(), None)
                }
            };
            items.push(StorageInfo {
                app_id,
                exists,
                path,
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
