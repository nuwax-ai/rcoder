use super::*;

impl KubernetesRuntime {
    /// scale Deployment replicas
    pub async fn scale_app(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        if replicas < 0 {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Replica count must be nonnegative".into(),
            ));
        }
        let identity = self.capture_app_mutation_identity(app_id).await?;
        if replicas > 0 {
            self.claim_app_storage(app_id).await?;
        }
        let name = &identity.name;
        let patch = serde_json::json!({ "spec": { "replicas": replicas } });
        self.patch_captured_app(&identity, patch).await?;
        info!("[K8S-APP] Deployment {name} scaled to {replicas}");
        Ok(())
    }

    /// patch Deployment 的闲置回收策略注解(strategic merge:只改指定注解键,不碰 pod template → 不触发 rollout)。
    /// 字段 None=不改该键;两者皆 None 由上层 service 校验拒绝(此处防御性早返 Ok)。
    pub async fn patch_app_recycle_policy(
        &self,
        app_id: &str,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> ContainerRuntimeResult<()> {
        let name = self.app_deployment_name(app_id);
        let mut ann = serde_json::Map::new();
        if let Some(b) = recycle_enabled {
            ann.insert(
                RECYCLE_ENABLED_ANNOTATION.to_string(),
                serde_json::Value::from(b.to_string()),
            );
        }
        if let Some(s) = idle_timeout_seconds {
            ann.insert(
                IDLE_TIMEOUT_ANNOTATION.to_string(),
                serde_json::Value::from(s.to_string()),
            );
        }
        if ann.is_empty() {
            return Ok(()); // 防御:两字段皆 None(service 层已校验)
        }
        let identity = self.capture_app_mutation_identity(app_id).await?;
        let patch = serde_json::json!({ "metadata": { "annotations": ann } });
        self.patch_captured_app(&identity, patch).await?;
        info!(
            "[K8S-APP] Deployment {name} recycle policy patched (enabled={:?}, idle_timeout={:?})",
            recycle_enabled, idle_timeout_seconds
        );
        Ok(())
    }

    pub async fn patch_app_wake_on_traffic(
        &self,
        app_id: &str,
        enabled: bool,
    ) -> ContainerRuntimeResult<()> {
        let identity = self.capture_app_mutation_identity(app_id).await?;
        let annotations =
            std::collections::BTreeMap::from([(WAKE_ON_TRAFFIC_ANNOTATION, enabled.to_string())]);
        let patch = serde_json::json!({ "metadata": { "annotations": annotations } });
        self.patch_captured_app(&identity, patch).await?;
        Ok(())
    }

    /// 触发滚动重启（rollout annotation）
    pub async fn restart_app(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let identity = self.capture_app_mutation_identity(app_id).await?;
        self.claim_app_storage(app_id).await?;
        let name = &identity.name;
        // kubectl 风格固定秒精度:注解值仅要求"变化即触发",但可变小数位
        // 是纳秒时间戳事故的同款模式(手拼时间戳一律定精度)。
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let patch = serde_json::json!({
            "spec": { "template": { "metadata": { "annotations": {
                "kubectl.kubernetes.io/restartedAt": now
            } } } }
        });
        self.patch_captured_app(&identity, patch).await?;
        info!("[K8S-APP] Deployment {name} restarted");
        Ok(())
    }

    /// Capture the physical target before any storage claim or workload mutation.
    async fn capture_app_mutation_identity(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
        let name = self.app_deployment_name(app_id);
        let deployment = self.deployments_api().get(&name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Read application mutation target: {error}"))
        })?;
        let expected = self.build_app_labels(app_id, None, None);
        if !deployment.metadata.labels.as_ref().is_some_and(|labels| {
            expected
                .iter()
                .all(|(key, value)| labels.get(key) == Some(value))
        }) {
            return Err(ContainerRuntimeError::Conflict(
                "Application mutation target ownership changed".into(),
            ));
        }
        app_mutation_identity(name, &deployment.metadata)
    }

    pub(crate) async fn patch_captured_app(
        &self,
        identity: &shared_types::AppResourceIdentity,
        patch: serde_json::Value,
    ) -> ContainerRuntimeResult<()> {
        let patch = condition_app_patch(identity, patch)?;
        self.deployments_api()
            .patch(
                &identity.name,
                &PatchParams::default(),
                &Patch::Merge(patch),
            )
            .await
            .map_err(|error| match &error {
                kube::Error::Api(response) if response.code == 409 => {
                    ContainerRuntimeError::Conflict(format!(
                        "Application mutation precondition failed: {error}"
                    ))
                }
                _ => crate::runtime::builder_completion::k8s_error(
                    format!("Patch captured application: {error}"),
                    error,
                ),
            })?;
        Ok(())
    }

    /// Same identity-fenced patch as [`Self::patch_captured_app`] but with the
    /// strategic merge type: `containers` entries merge by container name
    /// instead of the array being replaced atomically.
    pub(crate) async fn patch_captured_app_strategic(
        &self,
        identity: &shared_types::AppResourceIdentity,
        patch: serde_json::Value,
    ) -> ContainerRuntimeResult<()> {
        let patch = condition_app_patch(identity, patch)?;
        self.deployments_api()
            .patch(
                &identity.name,
                &PatchParams::default(),
                &Patch::Strategic(patch),
            )
            .await
            .map_err(|error| match &error {
                kube::Error::Api(response) if response.code == 409 => {
                    ContainerRuntimeError::Conflict(format!(
                        "Application mutation precondition failed: {error}"
                    ))
                }
                _ => crate::runtime::builder_completion::k8s_error(
                    format!("Patch captured application: {error}"),
                    error,
                ),
            })?;
        Ok(())
    }

    /// Remove obsolete port resources after a successful update.
    pub async fn cleanup_orphan_port_resources(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<()> {
        let has_http = params
            .ports
            .as_ref()
            .is_some_and(|ps| ps.iter().any(|p| p.expose_type == ExposeType::Http));
        let has_tcp = params
            .ports
            .as_ref()
            .is_some_and(|ps| ps.iter().any(|p| p.expose_type == ExposeType::Tcp));
        let has_env = params.env.as_ref().is_some_and(|e| !e.is_empty());
        let has_secrets = params.secrets.as_ref().is_some_and(|s| !s.is_empty());
        // step-D 写面 fencing：清理删除一律 uid+RV 前置（对齐 delete_captured）
        // ——接管后的迟到删除被前置拒绝，不会误删同名新代资源。
        if !has_http {
            let name = self.app_http_route_name(app_id);
            let api = self.httproute_api();
            if let Some(live) = api.get_opt(&name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get httproute {name} before cleanup: {e}"))
            })? {
                let dp = crate::runtime::k8s_runtime_helpers::conditioned_delete_params(
                    &live.metadata,
                    None,
                )?;
                self.ignore_404(api.delete(&name, &dp).await).await?;
            }
        }
        if !has_tcp {
            let name = self.app_nodeport_name(app_id);
            let api = self.services_api();
            if let Some(live) = api.get_opt(&name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get nodeport {name} before cleanup: {e}"))
            })? {
                let dp = crate::runtime::k8s_runtime_helpers::conditioned_delete_params(
                    &live.metadata,
                    None,
                )?;
                self.ignore_404(api.delete(&name, &dp).await).await?;
            }
        }
        if !has_env {
            let name = self.app_config_name(app_id);
            let api = self.configmaps_api();
            if let Some(live) = api.get_opt(&name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get configmap {name} before cleanup: {e}"))
            })? {
                let dp = crate::runtime::k8s_runtime_helpers::conditioned_delete_params(
                    &live.metadata,
                    None,
                )?;
                self.ignore_404(api.delete(&name, &dp).await).await?;
            }
        }
        if !has_secrets {
            let name = self.app_secret_name(app_id);
            let api = self.secrets_api();
            if let Some(live) = api.get_opt(&name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get secret {name} before cleanup: {e}"))
            })? {
                let dp = crate::runtime::k8s_runtime_helpers::conditioned_delete_params(
                    &live.metadata,
                    None,
                )?;
                self.ignore_404(api.delete(&name, &dp).await).await?;
            }
        }
        Ok(())
    }

    /// 等 app Pod 容器全部退出（按 rcoder.io/app-id label 轮询 Pod phase），best-effort：
    /// 容器退出（phase != Running）或 Pod 消失即返回；超时/API 错误仅 warn 不阻塞删除
    /// （app 复用共享 PVC 子目录，残留写入影响可控）。
    /// 仅容忍 404（视为已删除/幂等），其余 K8s 错误透传
    async fn ignore_404<T>(&self, r: Result<T, kube::Error>) -> ContainerRuntimeResult<()> {
        match r {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(ContainerRuntimeError::K8sError(format!("delete: {e}"))),
        }
    }
}
