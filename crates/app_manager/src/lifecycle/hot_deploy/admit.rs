use super::*;

/// R10：热部署协调预算与冷部署同一配置事实源（deploy_budget.
/// absolute_budget_secs）——固定 1800s 常量无法兑现配置的预算护栏。
fn hot_reconciliation_budget(config: &crate::config::DeployBudgetConfig) -> Duration {
    Duration::from_secs(config.absolute_budget_secs.max(1))
}

/// Artifact identity passed together so the hot coordinator cannot mix releases.
pub(crate) struct HotArtifact<'a> {
    pub url: &'a str,
    pub release_id: &'a str,
    pub sha256: &'a str,
}

impl AppService {
    /// 尝试热部署。`Ok(Some(()))` = 完成（调用方跳过换 Pod 链）；`Ok(None)` = 前置不满足，回退换 Pod；`Err` = 受理后失败（现场
    /// 保留）。409（已有部署在进行）如实上抛冲突。
    #[cfg(test)]
    pub(crate) async fn try_deploy_via_container_api(
        &self,
        app_id: &str,
        url: &str,
        release_id: &str,
        sha256: &str,
        request: &StartAppRequest,
    ) -> AppResult<Option<()>> {
        let guard = Arc::new(self.try_acquire_process_release_lock(app_id).await?);
        let result = self
            .try_deploy_via_container_api_with_guard(
                app_id,
                HotArtifact {
                    url,
                    release_id,
                    sha256,
                },
                request,
                guard.clone(),
                &uuid::Uuid::new_v4().to_string(),
            )
            .await;
        let guard = Arc::try_unwrap(guard)
            .map_err(|_| AppOperationError::Conflict("Hot executor still owns lease".into()))?;
        finish_hot_operation(guard, result).await
    }

    #[cfg(test)]
    pub(crate) async fn try_deploy_via_container_api_with_guard(
        &self,
        app_id: &str,
        artifact: HotArtifact<'_>,
        request: &StartAppRequest,
        operation: Arc<crate::service::AppOperationGuard>,
        operation_id: &str,
    ) -> AppResult<Option<()>> {
        let Some(task) = self
            .prepare_hot_deployment(app_id, artifact, request, operation, operation_id)
            .await?
        else {
            return Ok(None);
        };
        task.execute_with_credentials(request.pg.as_ref()).await
    }

    pub(crate) async fn prepare_hot_deployment(
        &self,
        app_id: &str,
        artifact: HotArtifact<'_>,
        request: &StartAppRequest,
        operation: Arc<crate::service::AppOperationGuard>,
        operation_id: &str,
    ) -> AppResult<Option<HotDeploymentTask>> {
        let HotArtifact {
            url,
            release_id,
            sha256,
        } = artifact;
        self.metadata
            .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref())
            .await?;
        // 前置：app 存在且 Running 且有可路由 IP
        let app: AppRuntimeInfo = match self.get_app(app_id).await {
            Ok(app) => app,
            Err(AppOperationError::NotFound(_)) => {
                info!(
                    "[APP] hot deploy fallback: app {app_id} not found (first deploy → pod path)"
                );
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        // 前置：容器配置了部署令牌（未配置 = 端点禁用 = 镜像未升级到 server 形态）
        let env_snapshot = self
            .runtime
            .app_env_snapshot(app_id)
            .await
            .map_err(|e| AppOperationError::Backend(format!("read hot deployment env: {e}")))?;
        if let Some(requested) = request.env.as_ref() {
            validate_requested_hot_env(requested, &env_snapshot.env)?;
        }
        let Some(ip) = app
            .health
            .instance
            .as_ref()
            .map(|instance| instance.ip.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            warn!("[APP] hot deploy fallback: app {app_id} has no ready runtime IP");
            return Ok(None);
        };

        let token = env_snapshot
            .env
            .get("APP_CLI_DEPLOY_TOKEN")
            .cloned()
            .filter(|token| !token.trim().is_empty());
        let Some(token) = token else {
            warn!(
                "[APP] hot deploy fallback: APP_CLI_DEPLOY_TOKEN not set on app {app_id} \
                 (server-form image required)"
            );
            return Ok(None);
        };

        // 受理
        let base = format!("http://{ip}:{APP_CLI_ADMIN_PORT}");
        let client = admin_client()?;
        let capability = client
            .get(format!("{base}/v1/deploy/status"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| {
                AppOperationError::Backend(format!("hot deployment capability probe failed: {e}"))
            })?;
        if capability.status().as_u16() == 404 {
            return Ok(None);
        }
        let capability: serde_json::Value = capability
            .error_for_status()
            .map_err(|e| {
                AppOperationError::Backend(format!("hot deployment capability status: {e}"))
            })?
            .json()
            .await
            .map_err(|e| {
                AppOperationError::Backend(format!("hot deployment capability body: {e}"))
            })?;
        if !supports_hot_protocol(&capability) {
            return Ok(None);
        }
        if request.pg.is_some()
            && !capability
                .pointer("/data/capabilities")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|values| {
                    values
                        .iter()
                        .any(|value| value.as_str() == Some("deployment_run_pg"))
                })
        {
            return Err(AppOperationError::Validation(
                "Hot deployment credentials require an app-cli supporting deployment_run_pg".into(),
            ));
        }
        let generation_id = env_snapshot
            .env
            .get(shared_types::APP_DEPLOY_GENERATION_ID)
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .ok_or_else(|| {
                AppOperationError::Backend("hot deployment generation missing".into())
            })?;
        // R10：预算来自配置（与冷部署同源），不再固定 1800s
        let budget = hot_reconciliation_budget(&self.config.deploy_budget);
        let task = HotDeploymentTask {
            runtime: self.runtime.clone(),
            access_mode: self.config.access_mode,
            app_id: app_id.to_owned(),
            url: url.to_owned(),
            release_id: release_id.to_owned(),
            sha256: sha256.to_owned(),
            base,
            token,
            env_snapshot,
            generation_id,
            operation_id: operation_id.to_owned(),
            operation,
            budget,
        };
        // Reconciliation may take up to the full stage budget; the HTTP caller's
        // 300s response timeout is handled by spawning the task and discarding the
        // JoinHandle after the caller gives up (see start_hot_deploy).
        Ok(Some(task))
    }
}
