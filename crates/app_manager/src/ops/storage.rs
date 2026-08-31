//! 持久存储管理（query/clear/destroy storage + orphan 检测，`app_stage` 显式分派 dev/prod）
//!
//! RBD 卷形态（rcoder 零挂载）：`exists` = PVC 归属（K8s label 集）/目录存在
//! （Docker）；`path` = PVC 名（K8s，非可挂载路径）/bind 源目录（Docker）；
//! `modified_at` 仅 Docker 可得（K8s 需容器运行，降级 None）。
//! - `prod`：`clear` K8s = 删 PVC（数据清空语义等价——RBD 不可挂载无法逐文件清，
//!   卷在下次 create 自动重建）；Docker = 清目录内容。
//! - `dev`：`clear` = 经容器 file-server 清空 workspace 内容（留容器留卷——
//!   开发容器常驻，"重置开发工作区"语义）；`destroy` = UserappDevCleanup 四步
//!   回收整个开发环境（容器+PVC+目录+注册），不动 metadata。

use tracing::{info, warn};

use shared_types::ServiceType;
use shared_types::UserappStage;

use crate::models::*;
use crate::utils::*;

/// app_stage → 卷形态的 ServiceType（K8s PVC label / Docker 目录树都按它分形）。
fn service_type_of(app_stage: UserappStage) -> ServiceType {
    match app_stage {
        UserappStage::Dev => ServiceType::UserappBuilder,
        UserappStage::Prod => ServiceType::Userapp,
    }
}

impl crate::service::AppService {
    // ===== 持久存储管理（v2 §5.4）=====
    // 删应用默认保留数据；这组接口让 Java 显式管理残留存储。
    // StorageInfo 不含 size_bytes——需容器运行时 exec du，跨面语义不稳（见设计文档 §5.4）。

    /// 查询单个应用的持久存储状态（prod=运行卷；dev=开发卷）。
    pub async fn get_app_storage(
        &self,
        app_stage: UserappStage,
        app_id: &str,
    ) -> AppResult<StorageInfo> {
        validate_app_id(app_id)?;
        let is_orphan = match app_stage {
            UserappStage::Prod => self.is_storage_orphan(app_id).await,
            UserappStage::Dev => self.is_dev_storage_orphan(app_id).await,
        };
        let (exists, path) = self.storage_path_info(app_stage, app_id).await?;
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
    async fn storage_path_info(
        &self,
        app_stage: UserappStage,
        app_id: &str,
    ) -> AppResult<(bool, std::path::PathBuf)> {
        let service_type = service_type_of(app_stage);
        let path = self
            .runtime
            .workspace_volume_name(app_id, &service_type)
            .await
            .map_err(|e| map_runtime_error("[APP] workspace_volume_name failed", e))?;
        let path = std::path::PathBuf::from(path);
        if shared_types::is_kubernetes_runtime() {
            // PVC 归属 = label 集（list_workspace_identifiers 枚举 per-app PVC，
            // 含已 delete 的孤儿）；path 字段是 PVC 名而非可挂载路径
            let exists = self
                .runtime
                .list_workspace_identifiers(&service_type)
                .await
                .map_err(|e| map_runtime_error("[APP] list_workspace_identifiers failed", e))?
                .contains(&app_id.to_string());
            Ok((exists, path))
        } else {
            // Docker：workspace_volume_name 返回的是展示标识（通配串，非可 stat
            // 路径）——存在性用元数据 uid 精确定位对应树的 workspace 段。
            let ws_dir = match app_stage {
                UserappStage::Prod => self.app_prod_dirs(app_id)[0].clone(),
                UserappStage::Dev => self.app_dev_dirs(app_id)[0].clone(),
            };
            let exists = tokio::fs::metadata(&ws_dir).await.is_ok();
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

    /// per-app prod 四目录的 rcoder 容器内锚点路径（`{锚点}/prod/{user_id}/` 下
    /// `{app_id}/ + data/{app_id}/ + logs/{app_id}/ + agent-store/{app_id}/`，bind
    /// 双向同步宿主——Docker 模式 clear 的四目录定位；布局单一事实源
    /// [`shared_types::paths::userapp_prod_subpaths`]）。owner user_id 查元数据，
    /// 缺失/空白兜底 app_id（与 docker_app_runtime 组装 bind 源的兜底一致）。
    fn app_prod_dirs(&self, app_id: &str) -> [std::path::PathBuf; 4] {
        let uid = self.app_owner_uid(app_id);
        shared_types::paths::userapp_prod_subpaths(&uid, app_id).map(|sub| {
            std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT).join(sub)
        })
    }

    /// per-app dev 四目录的 rcoder 容器内锚点路径（`{锚点}/dev/{user_id}/` 下与
    /// prod 同构四段；布局单一事实源 [`shared_types::paths::userapp_dev_subpaths`]）。
    fn app_dev_dirs(&self, app_id: &str) -> [std::path::PathBuf; 4] {
        let uid = self.app_owner_uid(app_id);
        shared_types::paths::userapp_dev_subpaths(&uid, app_id).map(|sub| {
            std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT).join(sub)
        })
    }

    /// app owner user_id（元数据查询；缺失/空白兜底 app_id——与 bind 源组装兜底一致）。
    fn app_owner_uid(&self, app_id: &str) -> String {
        self.metadata
            .lookup(app_id)
            .and_then(|r| r.user_id)
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| app_id.to_string())
    }

    /// 清空应用持久存储内容（数据语义；卷对象去留按运行时能力与环境语义）。
    /// - `prod`：安全约束仅当 app 计算资源已不存在时允许（否则 INVALID_STATE）。
    ///   K8s/RBD：rcoder 不可挂载无法逐文件清 → 删 PVC（下次 create 自动重建空卷，
    ///   数据清空语义等价；handbook 注明）。Docker：清 prod 四目录内容（留目录本身）。
    /// - `dev`：经开发容器 file-server 清空 workspace 内容（**留容器留卷**——
    ///   "重置开发工作区"语义；开发容器常驻，卷重建要求先销毁容器，得不偿失）。
    ///   幂等；容器内为旧镜像（无 clear 端点）时 404 上抛 Backend。
    pub async fn clear_app_storage(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<()> {
        validate_app_id(app_id)?;
        if app_stage == UserappStage::Dev {
            let base = self
                .app_files_base(UserappStage::Dev, app_id, Some(user_id))
                .await?;
            let resp = reqwest::Client::new()
                .post(format!("{base}/api/v1/userapp/app-files/clear"))
                .timeout(std::time::Duration::from_secs(60))
                .json(&serde_json::json!({"app_id": app_id, "user_id": user_id}))
                .send()
                .await
                .map_err(|e| {
                    AppOperationError::Backend(format!(
                        "forward clear-dev-workspace to dev container (app {app_id}): {e}"
                    ))
                })?;
            let checked =
                crate::ops::files::check_status(resp, "clear-dev-workspace", app_id).await?;
            drop(checked);
            info!(
                "[APP] dev workspace cleared (container/volume retained): {}",
                app_id
            );
            return Ok(());
        }
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
        // prod 四目录（workspace 发布制品 + PG/dbx 数据 + 日志 + agent-store）：
        // "清空持久存储"语义的主体；owner 查元数据（元数据行在 clear 场景恒保留），
        // 缺失兜底 app_id（与 bind 源组装的兜底一致）。
        for dir in self.app_prod_dirs(app_id) {
            if dir.exists()
                && let Err(e) = Self::purge_dir_contents(&dir).await
            {
                return Err(map_io_error("failed to clear app storage", e, false));
            }
        }
        info!("[APP] app storage cleared: {}", app_id);
        Ok(())
    }

    /// 销毁应用持久存储 PVC（高危·不可逆·释放配额）。
    ///
    /// - `prod`：安全约束 ① 仅当 app 计算资源已不存在（已 delete）时允许（否则
    ///   INVALID_STATE）；② body `confirm` 必须等于 app_id。K8s 删 PVC 对象，
    ///   Docker 等价删 bind 目录；元数据行同步删除（三档删除语义的第三档）。
    /// - `dev`：销毁**整个开发环境** = UserappDevCleanup 四步回收（builder 容器 +
    ///   dev PVC + Docker dev 目录 + 摘注册/探活缓存）；**不动 metadata**（owner
    ///   保留——create-workspace 幂等重建开发环境）。幂等：资源不存在视为成功。
    pub async fn destroy_app_storage(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        user_id: &str,
        confirm: &str,
    ) -> AppResult<()> {
        if app_stage == UserappStage::Dev {
            validate_app_id(app_id)?;
            if confirm != app_id {
                return Err(AppOperationError::Validation(format!(
                    "confirm must equal app_id for destroy (high-risk op): got confirm='{confirm}'"
                )));
            }
            // 显式 dev destroy 必须确定性执行（区别于 purge 链的 best-effort 联动）：
            // 未注入即硬错，不静默跳过
            let cleanup = self
                .dev_cleanup
                .read()
                .expect("dev_cleanup lock")
                .clone()
                .ok_or_else(|| {
                    AppOperationError::Backend("userapp dev cleanup not injected".to_string())
                })?;
            cleanup.cleanup(app_id).await.map_err(|e| {
                AppOperationError::Backend(format!(
                    "destroy userapp dev resources (app {app_id}): {e}"
                ))
            })?;
            info!(
                "[APP] userapp dev environment destroyed (metadata retained): {}",
                app_id
            );
            return Ok(());
        }
        self.destroy_app_storage_keep_metadata(app_id, confirm)
            .await?;
        // 独立 storage/destroy 接口：与 PVC 同生命周期，元数据行同步删除
        // （三档删除语义的第三档）。
        self.metadata.record_deleted(app_id).await;
        // user_id 卷分区定位：Docker 下宿主目录定位由 destroy_app_pvc 的
        // prod/*/ 通配扫描兜底（正确性不依赖 metadata），显式值用于对账与
        // 未来精确直删。
        info!("[APP] app PVC destroyed: {} (user_id={})", app_id, user_id);
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
        // K8s：删单卷 PVC（destroy 内兜底回收存量 `-data` PVC）。
        // Docker：destroy_app_pvc 删 prod 树该 app 四目录（通配 prod/*/ 一层，
        // 与 dev cleanup 同款模式）+ 旧 RCODER_WORKSPACE_ROOT 制品目录兜底——
        // 双形态"删持久卷"语义在此收口，本层不再单独删目录（防双删漂移）。
        // 失败不吞：destroy 是显式高危操作（confirm=app_id），残留即孤儿；
        // 失败时外层 record_deleted 未执行，幂等重试收敛。
        self.runtime
            .destroy_app_pvc(app_id)
            .await
            .map_err(|e| map_runtime_error("destroy_app_pvc failed", e))?;
        // Userapp 开发资源回收（UserappBuilder 开发容器 + per-app 开发 PVC）：
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
                    "[APP] dev cleanup not injected, skip UserappBuilder resources recycle: app_id={app_id}"
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

    /// 分页查询持久存储（强制分页，无全量模式；prod=运行卷，dev=开发卷）。
    /// 过滤：orphan_only、app_ids 生效；tenant_id/space_id 在无状态下不支持（rcoder 不持
    /// app→租户映射），提供则 warn 忽略。
    ///
    /// dev 的在跑/orphan 判定走逐项 `dev_container_alive`（builder 非 Deployment，
    /// 无法像 prod 一次 list 拿全集）：`orphan_only` 过滤阶段对候选逐项探测
    /// （显式成本），否则仅对当前页条目探测（≤page_size 次）。
    pub async fn query_storage(
        &self,
        app_stage: UserappStage,
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
        let service_type = service_type_of(app_stage);
        // dev 逐项 alive 探测通道（仅 Dev 使用；未注入时保守判"在"→非 orphan）
        let dev_locator = if app_stage == UserappStage::Dev {
            Some(
                self.dev_locator
                    .read()
                    .expect("dev_locator lock")
                    .clone()
                    .ok_or_else(|| {
                        AppOperationError::Backend("dev container locator not injected".to_string())
                    })?,
            )
        } else {
            None
        };
        // 现有 app 集合（供 prod 的 is_orphan），一次 list 调用；dev 不整集预取
        //（builder 非 Deployment，无整集接口——见上注释）
        let existing: std::collections::HashSet<String> = if app_stage == UserappStage::Prod {
            self.runtime
                .list_deployments()
                .await
                .map_err(|e| map_runtime_error("[APP] list_deployments failed", e))?
                .into_iter()
                .map(|s| s.app_id)
                .collect()
        } else {
            std::collections::HashSet::new()
        };
        // 候选 = 所有"有持久数据"的 app：枚举对应形态的 per-app PVC（含**已 delete
        // 但 PVC 保留的孤儿**）——这才是 orphan 检测的数据源。prod 再并入运行中的
        // app（existing 兜底；正常 running app 都有 PVC，已含）；dev 不并入（builder
        // 无卷属病态，注册表/容器清单不是卷事实源）。
        let mut entries: std::collections::HashSet<String> = self
            .runtime
            .list_workspace_identifiers(&service_type)
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
        let orphan_only = filters.orphan_only.unwrap_or(false);
        // dev 在跑判定（探测通道缺失时保守判"在"→非 orphan）
        async fn dev_alive(
            locator: &Option<std::sync::Arc<dyn shared_types::UserappDevLocator>>,
            app_id: &str,
        ) -> bool {
            match locator {
                Some(locator) => matches!(locator.dev_container_alive(app_id).await, Ok(true)),
                None => {
                    warn!(
                        "[APP] query_storage dev alive probe unavailable, conservatively treating as non-orphan: app_id={app_id}"
                    );
                    true
                }
            }
        }
        let filtered: Vec<String> = entries
            .into_iter()
            .filter(|app_id| {
                if let Some(ids) = app_ids_filter
                    && !ids.iter().any(|x| x == app_id)
                {
                    return false;
                }
                true
            })
            .collect();
        // orphan_only 过滤（显式成本：dev 对全部候选逐项探测）
        let filtered: Vec<String> = if orphan_only {
            let mut kept = Vec::new();
            for app_id in filtered {
                let is_orphan = if app_stage == UserappStage::Prod {
                    !existing.contains(&app_id)
                } else {
                    !dev_alive(&dev_locator, &app_id).await
                };
                if is_orphan {
                    kept.push(app_id);
                }
            }
            kept
        } else {
            filtered
        };
        let total = filtered.len() as u64;
        let page = request.page as usize;
        let page_size = request.page_size as usize;
        let start = page.saturating_sub(1) * page_size;
        let paged: Vec<String> = filtered.into_iter().skip(start).take(page_size).collect();

        let mut items = Vec::with_capacity(paged.len());
        for app_id in paged {
            let is_orphan = if app_stage == UserappStage::Prod {
                !existing.contains(&app_id)
            } else {
                !dev_alive(&dev_locator, &app_id).await
            };
            // 单项标识解析失败（瞬时 K8s API 抖动）不中断整个列表：warn + 标记 not exist
            let (exists, path) = match self.storage_path_info(app_stage, &app_id).await {
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

    /// 存储是否为孤儿（无对应运行应用）。Ok(None)=orphan；Ok(Some)/Err=非孤儿（保守）。
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

    /// dev 卷是否为孤儿（dev 卷/目录在而 builder 容器不在）。探测失败保守判非孤儿
    /// （与 prod 语义对齐——避免误删在用开发环境）。
    async fn is_dev_storage_orphan(&self, app_id: &str) -> bool {
        let Some(locator) = self.dev_locator.read().expect("dev_locator lock").clone() else {
            warn!(
                "[APP] is_dev_storage_orphan locator not injected, conservatively non-orphan: app_id={app_id}"
            );
            return false;
        };
        match locator.dev_container_alive(app_id).await {
            Ok(false) => true,
            Ok(true) => false,
            Err(e) => {
                warn!(
                    "[APP] is_dev_storage_orphan probe failed app_id={}: {} (conservatively non-orphan)",
                    app_id, e
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::{MockRuntime, test_service};

    /// storage 的 app_stage 分派落点：workspace_volume_name / list_workspace_identifiers
    /// 必须按 app_stage 换 ServiceType（dev→UserappBuilder / prod→Userapp）——K8s 卷
    /// label 与 Docker 目录树都按它分形，分派错即查错卷。
    #[tokio::test]
    async fn storage_env_dispatches_service_type() {
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(std::path::Path::new("/tmp/ws"), runtime.clone());

        service
            .get_app_storage(UserappStage::Prod, "app-1")
            .await
            .expect("prod storage");
        service
            .get_app_storage(UserappStage::Dev, "app-1")
            .await
            .expect("dev storage");

        let calls = runtime.volume_name_calls.get("app-1").expect("calls");
        assert_eq!(
            *calls,
            vec!["Userapp".to_string(), "UserappBuilder".to_string()],
            "prod 先查运行卷、dev 查开发卷（ServiceType 分派）"
        );
    }

    /// query 的 app_stage 分派：dev 清单枚举 UserappBuilder 卷（不并入 Deployment 集），
    /// prod 枚举 Userapp 卷（并入运行中 app 兜底）。
    #[tokio::test]
    async fn query_storage_env_selects_volume_family() {
        /// dev query 依赖 locator 做 builder 在跑探测——stub 恒"在"（非 orphan）
        struct StubDevLocator;
        #[async_trait::async_trait]
        impl shared_types::UserappDevLocator for StubDevLocator {
            async fn dev_file_server_addr(
                &self,
                _app_id: &str,
                _user_id: Option<&str>,
            ) -> Result<String, String> {
                Ok("http://127.0.0.1:60000".to_string())
            }
            async fn dev_container_alive(&self, _app_id: &str) -> Result<bool, String> {
                Ok(true)
            }
        }

        let runtime = Arc::new(MockRuntime::default());
        runtime
            .workspace_ids
            .insert("UserappBuilder".to_string(), vec!["app-dev".to_string()]);
        runtime
            .workspace_ids
            .insert("Userapp".to_string(), vec!["app-prod".to_string()]);
        let service = test_service(std::path::Path::new("/tmp/ws"), runtime.clone());
        *service.dev_locator.write().expect("dev_locator lock") = Some(Arc::new(StubDevLocator));

        let dev_resp = service
            .query_storage(
                UserappStage::Dev,
                QueryStorageRequest {
                    user_id: "u1".into(),
                    page: 1,
                    page_size: 10,
                    filters: None,
                },
            )
            .await
            .expect("dev query");
        assert_eq!(
            dev_resp
                .items
                .iter()
                .map(|i| i.app_id.as_str())
                .collect::<Vec<_>>(),
            vec!["app-dev"],
            "dev 清单只含开发卷"
        );

        let prod_resp = service
            .query_storage(
                UserappStage::Prod,
                QueryStorageRequest {
                    user_id: "u1".into(),
                    page: 1,
                    page_size: 10,
                    filters: None,
                },
            )
            .await
            .expect("prod query");
        assert_eq!(
            prod_resp
                .items
                .iter()
                .map(|i| i.app_id.as_str())
                .collect::<Vec<_>>(),
            vec!["app-prod"],
            "prod 清单只含运行卷"
        );
    }
}
