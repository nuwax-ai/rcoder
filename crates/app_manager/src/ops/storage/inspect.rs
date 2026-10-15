use super::*;

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
        // 权威生命周期记录缺失 = 应用从未存在（墓碑也保留 owner）：per-app
        // 存储只能随应用创建产生，直接判定不存在。存在记录但 owner 缺失仍走
        // 下游 InvalidState（异常数据不得静默）。存储读取错误照常上抛——
        // 只有权威"查无"能证明不存在，不能把存储故障当不存在。
        let authoritative_absent = self.metadata.lookup(app_id).await?.is_none();
        if authoritative_absent {
            return Ok(StorageInfo {
                app_id: app_id.to_string(),
                exists: false,
                path: String::new(),
                modified_at: None,
                is_orphan: false,
            });
        }
        let is_orphan = match app_stage {
            UserappStage::Prod => self.is_storage_orphan(app_id).await?,
            UserappStage::Dev => self.is_dev_storage_orphan(app_id).await?,
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
                UserappStage::Prod => self.app_prod_dirs(app_id).await?[0].clone(),
                UserappStage::Dev => self.app_dev_dirs(app_id).await?[0].clone(),
            };
            let exists = tokio::fs::try_exists(&ws_dir)
                .await
                .map_err(|error| map_io_error("inspect application storage", error, true))?;
            Ok((exists, path))
        }
    }

    /// 校验 app 计算资源已不存在（clear/destroy 共用前置：必须先 delete app，否则 INVALID_STATE）。
    /// `op` 用于错误描述（如 "clearing storage" / "destroying PVC"）。
    pub(super) async fn ensure_app_deleted(&self, app_id: &str, op: &str) -> AppResult<()> {
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
    /// Missing ownership is an error; never derive a destructive path from app_id as user_id.
    pub(super) async fn app_prod_dirs(&self, app_id: &str) -> AppResult<[std::path::PathBuf; 4]> {
        Ok(
            shared_types::paths::userapp_prod_subpaths(app_id).map(|sub| {
                #[cfg(feature = "deploy-host")]
                let root = app_manager_utils_host_root();
                #[cfg(not(feature = "deploy-host"))]
                let root =
                    std::path::PathBuf::from(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT);
                root.join(sub)
            }),
        )
    }

    /// per-app dev 四目录的 rcoder 容器内锚点路径（`{锚点}/dev/{user_id}/` 下与
    /// prod 同构四段；布局单一事实源 [`shared_types::paths::userapp_dev_subpaths`]）。
    pub(super) async fn app_dev_dirs(&self, app_id: &str) -> AppResult<[std::path::PathBuf; 4]> {
        Ok(
            shared_types::paths::userapp_dev_subpaths(app_id).map(|sub| {
                #[cfg(feature = "deploy-host")]
                let root = app_manager_utils_host_root();
                #[cfg(not(feature = "deploy-host"))]
                let root =
                    std::path::PathBuf::from(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT);
                root.join(sub)
            }),
        )
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
        let metadata = self.metadata.snapshot().await?;
        let service_type = service_type_of(app_stage);
        // dev 逐项 alive 探测通道（仅 Dev 使用；未注入时保守判"在"→非 orphan）
        let dev_locator = if app_stage == UserappStage::Dev {
            Some(
                self.dev_locator
                    .read()
                    .map_err(|_| AppOperationError::Backend("Dev locator lock poisoned".into()))?
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
        // dev 在跑判定（探测通道缺失时保守判"在"→非 orphan）。
        // 多实例协作模型：owner 实例回落档（orphan 全量语义见 destroy 链的
        // app-id 聚合扫描；此处列表按 owner 过滤，owner 视角自洽）。
        async fn dev_alive(
            locator: &Option<std::sync::Arc<dyn shared_types::UserappDevLocator>>,
            app_id: &str,
        ) -> AppResult<bool> {
            let locator = locator.as_ref().ok_or_else(|| {
                AppOperationError::Backend("Dev locator is not configured".into())
            })?;
            locator
                .dev_container_alive(app_id)
                .await
                .map_err(AppOperationError::Backend)
        }
        let filtered: Vec<String> = entries
            .into_iter()
            .filter(|app_id| {
                let Some(record) = metadata.get(app_id) else {
                    return false;
                };
                if filters
                    .tenant_id
                    .as_ref()
                    .is_some_and(|tenant| record.tenant_id.as_ref() != Some(tenant))
                    || filters
                        .space_id
                        .as_ref()
                        .is_some_and(|space| record.space_id.as_ref() != Some(space))
                {
                    return false;
                }
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
                    !dev_alive(&dev_locator, &app_id).await?
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
                !dev_alive(&dev_locator, &app_id).await?
            };
            let (exists, path) = self.storage_path_info(app_stage, &app_id).await?;
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

    /// Only a successful runtime query can establish absence.
    pub(crate) async fn is_storage_orphan(&self, app_id: &str) -> AppResult<bool> {
        Ok(self
            .runtime
            .get_deployment_status(app_id)
            .await
            .map_err(|error| map_runtime_error("query storage compute ownership", error))?
            .is_none())
    }

    async fn is_dev_storage_orphan(&self, app_id: &str) -> AppResult<bool> {
        let locator = self
            .dev_locator
            .read()
            .map_err(|_| AppOperationError::Backend("Dev locator lock poisoned".into()))?
            .clone()
            .ok_or_else(|| AppOperationError::Backend("Dev locator is not configured".into()))?;
        Ok(!locator
            .dev_container_alive(app_id)
            .await
            .map_err(AppOperationError::Backend)?)
    }
}
