use super::*;

#[cfg(feature = "kubernetes")]
#[async_trait]
impl WorkspaceRuntime for KubernetesRuntime {
    async fn workspace_volume_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        // RBD 卷 rcoder 不可挂载（无路径视角）——PVC 名即存储事实
        use crate::runtime::k8s_pvc::K8sPvcOps;
        self.workspace_pvc_name(identifier, service_type)
    }

    async fn resolve_workspace_path(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<String>> {
        // 阶段2: rcoder 静态 PV 挂 CephFS 根 → {RCODER_CEPHFS_ROOT}/{subvolumePath}
        // (subvolumePath 形如 /volumes/csi/<uuid>/<subuuid>, fs 根绝对路径)。
        // file-server 经此聚合路径访问 agent 数据 (tree/git/skills), 不启动 agent pod。
        let cephfs_root =
            std::env::var("RCODER_CEPHFS_ROOT").unwrap_or_else(|_| "/app/cephfs-root".to_string());
        let subvolume_path = self
            .resolve_subvolume_path(identifier, service_type)
            .await?;
        // subvolumePath 以 / 开头 (fs 绝对路径); trim 防御性处理确保单斜杠拼接
        let sub = subvolume_path.trim_start_matches('/');
        Ok(Some(format!("{cephfs_root}/{sub}")))
    }

    async fn resolve_workspace_path_by_pvcname(
        &self,
        pvc_name: &str,
    ) -> ContainerRuntimeResult<Option<String>> {
        // 阶段3 lazy mv: 与 resolve_workspace_path 同, 但用任意 PVC 名 (共享 PVC 如 rcoder-workspace)
        let cephfs_root =
            std::env::var("RCODER_CEPHFS_ROOT").unwrap_or_else(|_| "/app/cephfs-root".to_string());
        let subvolume_path = self.resolve_subvolume_path_by_pvcname(pvc_name).await?;
        let sub = subvolume_path.trim_start_matches('/');
        Ok(Some(format!("{cephfs_root}/{sub}")))
    }

    /// per-agent workspace PVC 存在性（混住判据读侧, 30s TTL 缓存）。
    ///
    /// PVC 存在 = 该用户已切 RBD（转发层路由依据）；不存在 = 共享用户。
    /// 只读不创建（ensure 会把共享用户误变 RBD 用户——判据必须走本方法）。
    /// TTL 取 30s：迁移 cutover 翻转后最多一个窗口内判据滞后（此时容器已
    /// 按 RBD 起，转发路径不受影响；共享残留窗口只影响"转发 vs 本地"选择）。
    async fn workspace_pvc_exists(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        use crate::runtime::k8s_pvc::K8sPvcOps;
        const TTL: std::time::Duration = std::time::Duration::from_secs(30);
        let cache_key = format!("{service_type}/{identifier}");
        if let Some((exists, state, at)) = self.workspace_exists_cache.read().await.get(&cache_key)
        {
            if at.elapsed() < TTL {
                return Ok(*exists);
            }
        }
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        // 单次 GET 同时取 exists + annotation(read_migration_annotation 会对
        // 同一 PVC 再发一次全量 GET——合并掉)。
        let (exists, state) = match self.pvcs().get_opt(&pvc_name).await {
            Ok(pvc) => {
                let value = pvc
                    .as_ref()
                    .and_then(|p| p.metadata.annotations.as_ref())
                    .and_then(|a| a.get(shared_types::WORKSPACE_MIGRATION_ANNOTATION))
                    .map(String::as_str);
                (
                    pvc.is_some(),
                    shared_types::WorkspaceMigrationState::from_annotation(value),
                )
            }
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "workspace_pvc_exists: get PVC '{pvc_name}' failed: {e}"
                )))
            }
        };
        self.workspace_exists_cache
            .write()
            .await
            .insert(cache_key, (exists, state, std::time::Instant::now()));
        Ok(exists)
    }

    /// 迁移状态（PVC annotation, 30s TTL 缓存——与 exists 同 cache 同口径,
    /// 一次 GET 同时得两个判据; annotation 缺失/无法识别 → NotMigrated）。
    async fn workspace_migration_state(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<shared_types::WorkspaceMigrationState> {
        use crate::runtime::k8s_pvc::K8sPvcOps;
        const TTL: std::time::Duration = std::time::Duration::from_secs(30);
        let cache_key = format!("{service_type}/{identifier}");
        if let Some((_, state, at)) = self.workspace_exists_cache.read().await.get(&cache_key) {
            if at.elapsed() < TTL {
                return Ok(*state);
            }
        }
        // 单次 GET 同时取 exists + annotation(此前 get_opt 后又全量 GET 同一
        // PVC, 每请求两次 API); miss 后**回写缓存**——此前只读不写导致
        // intercept 每请求都打 API, 快路径名不副实, 且 replacement 冷缓存
        // 对 RBD 用户永远 Conflict 也同根于此。
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        let (exists, state) = match self.pvcs().get_opt(&pvc_name).await {
            Ok(Some(pvc)) => {
                let value = pvc
                    .metadata
                    .annotations
                    .as_ref()
                    .and_then(|a| a.get(shared_types::WORKSPACE_MIGRATION_ANNOTATION))
                    .map(String::as_str);
                (
                    true,
                    shared_types::WorkspaceMigrationState::from_annotation(value),
                )
            }
            Ok(None) => (false, shared_types::WorkspaceMigrationState::NotMigrated),
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get PVC '{pvc_name}': {e}"
                )))
            }
        };
        self.workspace_exists_cache
            .write()
            .await
            .insert(cache_key, (exists, state, std::time::Instant::now()));
        Ok(state)
    }

    /// 翻迁移状态（patch PVC annotation; NotMigrated = 移除 annotation）。
    /// 成功后即时写缓存（否则 30s TTL 窗口内判据滞后——cutover 后首个
    /// 请求可能仍走共享卷）。PVC 不存在 → NotFound。
    async fn set_workspace_migration_state(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        state: shared_types::WorkspaceMigrationState,
    ) -> ContainerRuntimeResult<()> {
        use crate::runtime::k8s_pvc::K8sPvcOps;
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        let existing = self
            .pvcs()
            .get_opt(&pvc_name)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("get PVC '{pvc_name}': {e}")))?
            .ok_or_else(|| {
                ContainerRuntimeError::ContainerNotFound(format!(
                    "PVC '{pvc_name}' not found; run phase-a before flipping migration state"
                ))
            })?;
        let uid = existing.metadata.uid.clone().ok_or_else(|| {
            ContainerRuntimeError::Conflict(format!("PVC '{pvc_name}' has no UID"))
        })?;
        let rv = existing.metadata.resource_version.clone().ok_or_else(|| {
            ContainerRuntimeError::Conflict(format!("PVC '{pvc_name}' has no resourceVersion"))
        })?;
        // NotMigrated 必须用显式 null 删除 key——RFC 7386 下空对象 {} 是
        // **无操作**(不删任何 key), 之前的 annotations:{} 导致 rollback 只
        // 改缓存、PVC 上 annotation 保留, 30s TTL 过后判据回弹 migrated
        // (静默假回滚, 比不提供 rollback 更危险)。
        let annotations = match state.as_annotation() {
            Some(value) => serde_json::json!({
                shared_types::WORKSPACE_MIGRATION_ANNOTATION: value
            }),
            None => serde_json::json!({
                shared_types::WORKSPACE_MIGRATION_ANNOTATION: serde_json::Value::Null
            }),
        };
        let patch = serde_json::json!({
            "metadata": {"uid": uid, "resourceVersion": rv, "annotations": annotations}
        });
        self.pvcs()
            .patch(
                &pvc_name,
                &kube::api::PatchParams::default(),
                &kube::api::Patch::Merge(&patch),
            )
            .await
            .map_err(|e| match &e {
                kube::Error::Api(response) if response.code == 404 => {
                    ContainerRuntimeError::ContainerNotFound(format!("PVC '{pvc_name}' vanished"))
                }
                kube::Error::Api(response) if response.code == 409 => {
                    ContainerRuntimeError::Conflict(format!(
                        "PVC '{pvc_name}' changed concurrently; retry"
                    ))
                }
                other => ContainerRuntimeError::K8sError(format!(
                    "patch PVC '{pvc_name}' migration state: {other}"
                )),
            })?;
        // 即时写缓存: cutover 后首个请求就要按新判据路由
        self.workspace_exists_cache.write().await.insert(
            format!("{service_type}/{identifier}"),
            (true, state, std::time::Instant::now()),
        );
        Ok(())
    }

    async fn workspace_volume_attached(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        self.workspace_volume_attached_inner(identifier, service_type)
            .await
    }

    async fn run_workspace_migration_sync(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        mode: shared_types::WorkspaceSyncMode,
        src_pvc: &str,
    ) -> ContainerRuntimeResult<()> {
        self.run_workspace_migration_sync_inner(identifier, service_type, mode, src_pvc)
            .await
    }

    /// 枚举某 service_type 的所有 per-app PVC，从 PVC 名反解 identifier（app_id）。
    ///
    /// 用于 storage/query 发现"有持久数据"的 app（含已 delete 但 PVC 保留的孤儿）——
    /// `list_deployments` 只能拿运行中的，PVC 才是持久数据的真源。
    /// PVC 名格式见 `workspace_pvc_name`：`{sanitize(container_prefix)}-{identifier(_→-)}-workspace`，
    /// identifier 经 app_id 校验已是 DNS-1123（无下划线），故反解结果即原 identifier。
    async fn list_workspace_identifiers(
        &self,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Vec<String>> {
        let selector = format!("service_type={}", service_type);
        let list = self
            .pvcs()
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!(
                    "list_workspace_identifiers: list PVC failed (service_type={}): {}",
                    service_type, e
                ))
            })?;
        // 前缀/后缀与 workspace_pvc_name 保持一致，反解中间段为 identifier
        let prefix = format!(
            "{}-",
            Self::sanitize_k8s_name_part(&self.service_container_prefix(service_type)?)
        );
        let suffix = "-workspace";
        let mut ids = Vec::with_capacity(list.items.len());
        for pvc in list.items {
            if let Some(name) = pvc.metadata.name.as_deref()
                && let Some(mid) = name
                    .strip_prefix(prefix.as_str())
                    .and_then(|s| s.strip_suffix(suffix))
            {
                ids.push(mid.to_string());
            }
        }
        Ok(ids)
    }

    async fn ensure_workspace(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        // 复用 K8sPvcOps::ensure_workspace_pvc (幂等: active→复用 / not_found→创建)
        self.ensure_workspace_pvc(identifier, service_type, storage_size)
            .await?;
        // 缓存即时失效: PVC 此刻必存在, 写 true——否则刚创建的 volume 在
        // workspace_pvc_exists 的 30s TTL 窗口内仍可能读到 false（新用户
        // 首个文件请求会落本地共享卷读到空数据）。
        use crate::runtime::k8s_pvc::K8sPvcOps;
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        let state = self
            .read_migration_annotation(&pvc_name)
            .await
            .unwrap_or(shared_types::WorkspaceMigrationState::NotMigrated);
        self.workspace_exists_cache.write().await.insert(
            format!("{service_type}/{identifier}"),
            (true, state, std::time::Instant::now()),
        );
        Ok(())
    }

    async fn destroy_app_pvc(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        // 委派 K8sPvcOps::destroy_workspace_pvc (service_type=Userapp; 仅 Userapp 走此路径,
        // agent PVC 永不删)。trait 方法默认 no-op, Docker 不覆盖。
        // 显式消歧: WorkspaceRuntime trait 也定义了同名方法(见下)。
        let snapshot = self.capture_deletion(app_id, None).await?;
        if snapshot
            .resources
            .iter()
            .any(|r| r.kind == shared_types::AppResourceKind::Deployment)
        {
            return Err(ContainerRuntimeError::Conflict(
                "application compute must be deleted before storage".into(),
            ));
        }
        // 兜底回收存量第二块 `-data` PVC（单卷化前的旧布局；新部署不存在=幂等 no-op）。
        // 失败不吞：半清理状态（数据卷残留=孤儿计费）比整体失败更难对账。
        self.delete_captured(&snapshot, true).await
    }

    async fn destroy_app_storage_snapshot(
        &self,
        snapshot: &shared_types::AppDeletionSnapshot,
    ) -> ContainerRuntimeResult<()> {
        self.delete_captured(snapshot, true).await
    }

    async fn destroy_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        // 消歧: 显式调 K8sPvcOps 同名方法（per-agent PVC 删除的实际实现）
        K8sPvcOps::destroy_workspace_pvc(self, identifier, service_type).await
    }

    async fn capture_app_storage_resize(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppStorageResizeTarget>> {
        self.capture_storage_resize(context).await.map(Some)
    }

    async fn resize_app_storage_target(
        &self,
        target: &shared_types::UserAppStorageResizeTarget,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome> {
        self.resize_storage_target(target, new_size).await
    }

    async fn resize_app_storage(
        &self,
        app_id: &str,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome> {
        // 委派 K8sPvcOps::resize_app_pvc（读当前值→比较→patch/事实拒绝）。
        // trait 方法默认 no-op, Docker 不覆盖（bind 目录无容量语义）。
        K8sPvcOps::resize_app_pvc(self, app_id, new_size).await
    }
}

impl KubernetesRuntime {
    /// per-agent 卷判据的**同步**版（best-effort, 供 replacement 等同步构造
    /// 路径）：只读 30s TTL 缓存, 未命中/不确定 → false（保守共享路径）。
    /// 权威判定在 async create 编排（查 PVC annotation, 见
    /// k8s_agent_create 的 per_agent_volume 计算）。
    pub(crate) fn cached_per_agent_volume(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> bool {
        if !matches!(
            service_type,
            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject
        ) {
            return false;
        }
        // subvolume 变体无 migrated 概念: 开关开即 per-agent
        if shared_types::per_agent_pvc_enabled() {
            return true;
        }
        if !shared_types::per_agent_rbd_enabled() {
            return false;
        }
        match self.workspace_exists_cache.try_read() {
            Ok(cache) => matches!(
                cache.get(&format!("{service_type}/{identifier}")),
                Some((true, shared_types::WorkspaceMigrationState::Migrated, sampled))
                    if sampled.elapsed() < std::time::Duration::from_secs(30)
            ),
            Err(_) => false,
        }
    }

    /// 读 PVC 的迁移 annotation（无法识别的值 → NotMigrated）。私有 helper
    /// （非 WorkspaceRuntime 接口——内部实现细节）。
    async fn read_migration_annotation(
        &self,
        pvc_name: &str,
    ) -> ContainerRuntimeResult<shared_types::WorkspaceMigrationState> {
        let pvc =
            self.pvcs().get(pvc_name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get PVC '{pvc_name}': {e}"))
            })?;
        let value = pvc
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(shared_types::WORKSPACE_MIGRATION_ANNOTATION))
            .map(String::as_str);
        Ok(shared_types::WorkspaceMigrationState::from_annotation(
            value,
        ))
    }
}
