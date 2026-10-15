use super::*;

pub(crate) struct HotDeploymentTask {
    pub(super) runtime: Arc<dyn UserAppRuntime>,
    pub(super) access_mode: AppAccessMode,
    pub(super) app_id: String,
    pub(super) url: String,
    pub(super) release_id: String,
    pub(super) sha256: String,
    pub(super) base: String,
    pub(super) token: String,
    pub(super) env_snapshot: shared_types::AppEnvSnapshot,
    pub(super) generation_id: String,
    pub(super) operation_id: String,
    pub(super) operation: Arc<crate::service::AppOperationGuard>,
    /// R10：协调预算（受理时从 config 解析传入——execute 上下文无 config）
    pub(super) budget: Duration,
}

impl HotDeploymentTask {
    /// Persist this identity before submitting the owner write. An IP and a
    /// deployment generation alone do not identify the pod/container that ran it.
    pub(crate) async fn capture_recovery_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> AppResult<shared_types::RuntimeConfigurationTarget> {
        if context.app_id != self.app_id || context.operation_id != self.operation_id {
            return Err(AppOperationError::Conflict(
                "Hot deployment recovery identity differs from its execution".into(),
            ));
        }
        self.runtime
            .capture_app_configuration_target(context, &self.generation_id)
            .await
            .map_err(|error| {
                AppOperationError::Backend(format!(
                    "Capture hot deployment physical target: {error}"
                ))
            })
    }

    pub(crate) async fn execute_captured(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        pg: Option<&shared_types::StartPgCredential>,
        owned: &mut crate::service::OwnedOperation,
    ) -> AppResult<Option<()>> {
        if self.capture_recovery_target(context).await? != *target {
            return Err(AppOperationError::Conflict(
                "Hot deployment physical target changed before submission".into(),
            ));
        }
        let result = tokio::time::timeout(self.budget, self.execute_inner(pg, Some(owned)))
            .await
            .map_err(|_| {
                AppOperationError::Backend("Hot deployment execution deadline exceeded".into())
            })??;
        if self.capture_recovery_target(context).await? != *target {
            return Err(AppOperationError::Conflict(
                "Hot deployment physical target changed before completion; reconcile original operation"
                    .into(),
            ));
        }
        Ok(result)
    }

    #[cfg(test)]
    pub(super) async fn run(self) -> AppResult<Option<()>> {
        let result = self.execute().await;
        if let Err(error) = &result {
            warn!(app_id = %self.app_id, operation_id = %self.operation_id, %error,
                "Hot deployment coordinator stopped with an error");
        }
        let operation = Arc::try_unwrap(self.operation)
            .map_err(|_| AppOperationError::Conflict("Hot operation still has observers".into()))?;
        finish_hot_operation(operation, result).await
    }

    #[cfg(test)]
    async fn execute(&self) -> AppResult<Option<()>> {
        self.execute_with_credentials(None).await
    }

    #[cfg(test)]
    pub(crate) async fn execute_with_credentials(
        &self,
        pg: Option<&shared_types::StartPgCredential>,
    ) -> AppResult<Option<()>> {
        tokio::time::timeout(self.budget, self.execute_inner(pg, None))
            .await
            .map_err(|_| {
                AppOperationError::Backend("Hot deployment execution deadline exceeded".into())
            })?
    }

    async fn execute_inner(
        &self,
        pg: Option<&shared_types::StartPgCredential>,
        owned: Option<&mut crate::service::OwnedOperation>,
    ) -> AppResult<Option<()>> {
        let Self {
            runtime,
            access_mode,
            app_id,
            url,
            release_id,
            sha256,
            base,
            token,
            env_snapshot,
            generation_id,
            operation_id,
            operation,
            budget,
        } = self;
        let access_mode = *access_mode;
        let body = serde_json::json!({
            "operation_id": operation_id,
            "deployment_generation_id": generation_id,
            "url": url,
            "release_id": release_id,
            "sha256": if sha256.is_empty() { None } else { Some(sha256) },
            "pg": pg,
        });
        operation.mark_mutating()?;
        let resp = admin_client()?
            .post(format!("{base}/v1/deploy"))
            .timeout(Duration::from_secs(30))
            .header("X-Deploy-Token", token.as_str())
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().as_u16() == 202 => {}
            Ok(r) if r.status().as_u16() == 409 => {
                operation.mark_rejected_before_mutation();
                // 透传容器侧错误体（含 "deploy in progress (phase=...)" 真实相位）
                let detail = r.text().await.map_err(|error| {
                    AppOperationError::Backend(format!("read hot deployment rejection: {error}"))
                })?;
                return Err(AppOperationError::Conflict(format!(
                    "hot deploy rejected by container: {detail}"
                )));
            }
            Ok(r) => {
                if matches!(r.status().as_u16(), 400 | 401 | 403 | 404 | 405 | 422) {
                    operation.mark_rejected_before_mutation();
                }
                return Err(AppOperationError::Backend(format!(
                    "hot deployment request rejected: HTTP {}",
                    r.status()
                )));
            }
            Err(e) => {
                return Err(AppOperationError::Backend(format!(
                    "hot deployment acceptance uncertain; inspect operation {operation_id} before retry: {e}"
                )));
            }
        }

        info!("[APP] hot deploy accepted: app_id={app_id}, release_id={release_id}");

        // Completion requires the matched operation and independent business readiness.
        let client = admin_client()?;
        // R10：受理时绑定的同一预算（config 事实源；不再固定 1800s）
        let deadline = tokio::time::Instant::now() + *budget;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AppOperationError::Backend(
                    "hot deployment reconciliation deadline exceeded; ownership retained; inspect deployment status before retry"
                        .to_string(),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            let probe = self.read_ready_operation(&client).await?;
            // 穷尽 match：新增 AppCliDeployPhase 变体时编译错，强制同步本判据
            match probe.as_ref().map(|p| p.phase) {
                Some(AppCliDeployPhase::Running) => break,
                Some(AppCliDeployPhase::Failed) => {
                    let error = format!(
                        "hot deploy operation {operation_id} failed: {}",
                        probe
                            .as_ref()
                            .and_then(|p| p.error.as_deref())
                            .unwrap_or("see app-cli logs for recovery outcome")
                    );
                    return Err(AppOperationError::Backend(error));
                }
                // deploying/orchestrating（换应用进行中）、idle（受理竞态窗口）、
                // None（不可达/未知相位——旧镜像）→ 继续等
                Some(
                    AppCliDeployPhase::Idle
                    | AppCliDeployPhase::Deploying
                    | AppCliDeployPhase::Orchestrating,
                )
                | None => {}
            }
        }

        // 收敛 env 三元组进 ConfigMap（不触发 Recreate）：Pod 重建恢复最新版本。
        // 目标先入检查点：begin_hot_convergence 与 ConfigMap 写之间崩溃时，
        // 恢复观察者有可比对的期望值。
        let mut env = env_snapshot.env.clone();
        crate::release_flow::identity::strip_release_identity(&mut env);
        env.insert("APP_DEPLOY_URL".into(), url.clone());
        env.insert("APP_RELEASE_ID".into(), release_id.clone());
        env.insert("APP_DEPLOY_SHA256".into(), sha256.clone());
        env.insert(
            shared_types::APP_DEPLOY_OPERATION_ID.into(),
            operation_id.clone(),
        );
        if let Some(owned) = owned {
            owned.begin_hot_convergence(&env).await?;
            owned.authorize_mutation().await?;
        }
        converge_deploy_env_after_hot(
            runtime.as_ref(),
            access_mode,
            app_id,
            env_snapshot,
            &env,
            operation,
        )
        .await?;
        info!("[APP] hot deploy done (pod kept): app_id={app_id}, release_id={release_id}");
        Ok(Some(()))
    }
    pub(super) async fn read_ready_operation(
        &self,
        client: &reqwest::Client,
    ) -> AppResult<Option<shared_types::AppDeploymentOperation>> {
        let probe = self.read_operation(client).await?;
        if probe
            .as_ref()
            .is_some_and(|operation| operation.phase == AppCliDeployPhase::Running)
        {
            // Readiness has no operation identity: fence it with status reads on
            // both sides so an intervening deployment cannot confirm this operation.
            if self.application_ready(client).await? {
                return self.read_operation(client).await;
            }
            return Ok(None);
        }
        Ok(probe)
    }

    async fn read_operation(
        &self,
        client: &reqwest::Client,
    ) -> AppResult<Option<shared_types::AppDeploymentOperation>> {
        let response = client
            .get(format!("{}/v1/deploy/status", self.base))
            .timeout(Duration::from_secs(10))
            .header("X-Deploy-Token", self.token.as_str())
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                let document = response
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|error| {
                        AppOperationError::Backend(format!(
                            "invalid hot deployment status: {error}"
                        ))
                    })?;
                Ok(observe_hot_operation(
                    &document,
                    &self.operation_id,
                    &self.release_id,
                    &self.generation_id,
                    &self.operation,
                ))
            }
            Ok(response) if response.status().is_client_error() => Err(AppOperationError::Backend(
                format!("hot deployment status rejected: HTTP {}", response.status()),
            )),
            _ => Ok(None),
        }
    }

    async fn application_ready(&self, client: &reqwest::Client) -> AppResult<bool> {
        let response = client
            .get(format!("{}/ready", self.base))
            .timeout(Duration::from_secs(10))
            .header("X-Deploy-Token", self.token.as_str())
            .send()
            .await;
        match response {
            Ok(response) if response.status() == reqwest::StatusCode::OK => {
                let document = response
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|error| {
                        AppOperationError::Backend(format!(
                            "invalid hot deployment readiness: {error}"
                        ))
                    })?;
                if document.get("status").and_then(serde_json::Value::as_str) != Some("ready") {
                    return Err(AppOperationError::Backend(
                        "invalid successful hot deployment readiness response".into(),
                    ));
                }
                Ok(document.get("phase").and_then(serde_json::Value::as_str) == Some("running"))
            }
            Ok(response) if response.status().is_client_error() => {
                Err(AppOperationError::Backend(format!(
                    "hot deployment readiness rejected: HTTP {}",
                    response.status()
                )))
            }
            _ => Ok(false),
        }
    }
}
