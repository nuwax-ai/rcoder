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
        if let Some((exists, at)) = self.workspace_exists_cache.read().await.get(&cache_key) {
            if at.elapsed() < TTL {
                return Ok(*exists);
            }
        }
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        let exists = self
            .pvcs()
            .get_opt(&pvc_name)
            .await
            .map(|pvc| pvc.is_some())
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!(
                    "workspace_pvc_exists: get PVC '{pvc_name}' failed: {e}"
                ))
            })?;
        self.workspace_exists_cache
            .write()
            .await
            .insert(cache_key, (exists, std::time::Instant::now()));
        Ok(exists)
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
        self.workspace_exists_cache
            .write()
            .await
            .insert(
                format!("{service_type}/{identifier}"),
                (true, std::time::Instant::now()),
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
