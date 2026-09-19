//! One durable operation owns deployment, configuration overrides and completion.
//! Private inputs are resolved before admission; execution never admits child controls.
use crate::models::*;
use crate::service::{AppOperationGuard, AppService, OwnedOperation};
use crate::utils::map_runtime_error;
use serde::{Deserialize, Serialize};
use shared_types::{UserAppControlCommand, UserAppExecutionInput, UserAppOperationKind};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeployInput {
    version: u32,
    request: StartAppRequest,
    params: Option<container_runtime_api::ContainerCreateParams>,
    previous: Option<container_runtime_api::DeploymentStatus>,
    restart: bool,
}
impl DeployInput {
    fn encode(&self) -> AppResult<UserAppExecutionInput> {
        let mut value = serde_json::to_value(self)
            .map_err(|_| AppOperationError::Backend("Encode deployment input".into()))?;
        value.sort_all_objects();
        Ok(UserAppExecutionInput::new(
            serde_json::to_string(&value)
                .map_err(|_| AppOperationError::Backend("Encode deployment input".into()))?,
        ))
    }
    pub(super) fn decode(input: &UserAppExecutionInput, restart: bool) -> AppResult<Self> {
        let input: Self = serde_json::from_str(input.encoded())
            .map_err(|_| AppOperationError::Backend("Decode deployment input".into()))?;
        if input.restart != restart
            || input.version != 1
            || input.params.as_ref().is_some_and(|params| {
                params.execution_context.is_some()
                    || params.mutation_target.is_some()
                    || params.service_type != shared_types::ServiceType::Userapp
            })
        {
            return Err(AppOperationError::InvalidState(
                "Unsupported deployment execution input".into(),
            ));
        }
        Ok(input)
    }
}

/// 整请求指纹：完整 StartAppRequest 的规范化字节 sha256（对象键序无关）。
/// 快路径与锁内受理共用，单一事实源。
fn deploy_fingerprint(request: &StartAppRequest) -> AppResult<String> {
    use sha2::Digest as _;
    Ok(hex::encode(sha2::Sha256::digest(
        shared_types::encode_userapp_intent(request)
            .map_err(|_| AppOperationError::Backend("Encode deployment intent".into()))?,
    )))
}

fn kind_from_restart(restart: bool) -> UserAppOperationKind {
    if restart {
        UserAppOperationKind::RestartDeployment
    } else {
        UserAppOperationKind::StartDeployment
    }
}

fn control_request(request: &StartAppRequest) -> shared_types::UserAppControlRequest {
    shared_types::UserAppControlRequest {
        lifecycle_id: request.lifecycle_id.clone(),
        request_id: request.request_id.clone(),
    }
}

/// 回放已成功操作的持久化完整响应（deployment_completed checkpoint）。
fn decode_stored_deploy_result(
    previous: &shared_types::UserAppOperationRecord,
) -> AppResult<StartAppResult> {
    serde_json::from_value(previous.checkpoint.clone())
        .map_err(|_| AppOperationError::Backend("Stored deployment result is unavailable".into()))
}

impl AppService {
    pub(crate) async fn replace_captured_runtime_configuration(
        &self,
        current: container_runtime_api::DeploymentStatus,
        operation: &mut OwnedOperation,
        guard: &AppOperationGuard,
    ) -> AppResult<()> {
        let context = operation.execution_context();
        let captured = self
            .runtime_configuration
            .operation_runtime_configuration(&context)
            .await?
            .ok_or_else(|| {
                AppOperationError::InvalidState(
                    "Runtime configuration was not captured at admission".into(),
                )
            })?;
        if self
            .metadata
            .store
            .operation_deadline(&context.app_id, &context.operation_id)
            .await?
            .is_none()
        {
            let budget = i64::try_from(self.config.deploy_budget.absolute_budget_secs)
                .ok()
                .and_then(|seconds| seconds.checked_mul(1000))
                .ok_or_else(|| {
                    AppOperationError::Validation(
                        "Deployment deadline exceeds supported range".into(),
                    )
                })?;
            self.metadata
                .store
                .bind_operation_deadline(
                    &context.app_id,
                    &context.operation_id,
                    &context.lifecycle_id,
                    chrono::Utc::now().timestamp_millis().saturating_add(budget),
                )
                .await?;
        }
        let mut params = self
            .build_container_params_from_update(
                &context.app_id,
                &UpdateAppRequest {
                    request_id: None,
                    lifecycle_id: Some(context.lifecycle_id.clone()),
                    name: None,
                    image: None,
                    env: None,
                    secrets: None,
                    resources: None,
                    tenant_id: None,
                    space_id: None,
                    expected_resource_version: current.resource_version.clone(),
                    recycle_enabled: None,
                    idle_timeout_seconds: None,
                },
                &current,
            )
            .await?;
        inject_captured_configuration(&mut params, &captured, &context, false);
        self.execute_update(&context.app_id, params, current, operation, guard)
            .await?;
        self.complete_cold_runtime_configuration(&context).await
    }

    pub(super) async fn deploy_controlled(
        &self,
        app_id: &str,
        request: StartAppRequest,
        restart: bool,
    ) -> AppResult<StartAppResult> {
        super::start::validate_start_request(app_id, &request)?;
        let guard = Arc::new(self.try_acquire_process_release_lock(app_id).await?);
        let result = self
            .deploy_admitted(app_id, request, restart, guard.clone())
            .await;
        let guard = Arc::try_unwrap(guard).map_err(|_| {
            AppOperationError::Conflict("Deployment executor still owns its resource lease".into())
        })?;
        if result.is_ok() || !guard.has_unfinished_mutation() {
            guard.finish().await?;
        }
        result
    }

    async fn deploy_admitted(
        &self,
        app_id: &str,
        request: StartAppRequest,
        restart: bool,
        guard: Arc<AppOperationGuard>,
    ) -> AppResult<StartAppResult> {
        self.metadata
            .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref())
            .await?;
        let identity = self.metadata.store.ensure_identity(app_id).await?;
        let fingerprint = deploy_fingerprint(&request)?;
        let kind = kind_from_restart(restart);
        if let Some(previous) = self
            .replay_control(app_id, &control_request(&request), kind, &fingerprint)
            .await?
        {
            return decode_stored_deploy_result(&previous);
        }
        let operation_id = uuid::Uuid::new_v4().to_string();
        let input = self
            .prepare_deploy_input(app_id, request, restart, &operation_id)
            .await?;
        let encoded = input.encode()?;
        let policy = shared_types::UserAppRuntimePolicy {
            recycle_enabled: input.request.idle_timeout_seconds.map(|value| value > 0),
            idle_timeout_seconds: input.request.idle_timeout_seconds,
            wake_on_traffic: Some(true),
        };
        let mut operation = OwnedOperation::admit_with_configuration(
            self.metadata.store.clone(),
            shared_types::UserAppAdmission {
                runtime_policy_on_success: Some(policy),
                metadata: input
                    .previous
                    .is_none()
                    .then(|| shared_types::UserAppMetadataPatch {
                        app_id: app_id.into(),
                        lifecycle_id: identity.lifecycle_id.clone(),
                        expected_revision: identity.metadata_revision,
                        name: Some(Some(app_id.into())),
                        tenant_id: None,
                        space_id: None,
                    }),
                command: Some(UserAppControlCommand::Deploy {
                    restart,
                    input_digest: encoded.digest(),
                }),
                app_id: app_id.into(),
                lifecycle_id: Some(identity.lifecycle_id),
                operation_id: operation_id.clone(),
                request_id: input.request.request_id.clone(),
                request_fingerprint: fingerprint,
                kind,
            },
            Some(&encoded),
            input.request.pg.as_ref(),
        )
        .await?;
        // Bind absolute deadline as a side-record (bind-once, before any runtime
        // side-effect). Failure here is fail-closed: if we cannot persist the
        // deadline, we must not proceed with the deploy.
        {
            let context = operation.execution_context();
            let now_ms = chrono::Utc::now().timestamp_millis();
            let absolute_ms = (self.config.deploy_budget.absolute_budget_secs as i64) * 1000;
            self.metadata
                .store
                .bind_operation_deadline(
                    &context.app_id,
                    &context.operation_id,
                    &context.lifecycle_id,
                    now_ms.saturating_add(absolute_ms),
                )
                .await?;
        }
        let result = self
            .execute_deploy_input(app_id, input, &mut operation, guard.clone())
            .await;
        match result {
            Ok(result) => {
                operation.succeed().await?;
                guard.mark_completed();
                self.activity.mark_running(app_id);
                Ok(result)
            }
            Err(error) => {
                if guard.has_unfinished_mutation() {
                    operation.fail(&error).await?;
                } else {
                    operation.reject_without_mutation(&error).await?;
                }
                Err(error)
            }
        }
    }

    async fn prepare_deploy_input(
        &self,
        app_id: &str,
        request: StartAppRequest,
        restart: bool,
        operation_id: &str,
    ) -> AppResult<DeployInput> {
        let mut request = self.validate_hot_env(app_id, request).await?;
        request.url = request.url.map(|url| url.trim().to_owned());
        request.release_id = request.release_id.map(|id| id.trim().to_owned());
        let previous = self
            .runtime
            .get_deployment_status(app_id)
            .await
            .map_err(|error| map_runtime_error("Read deployment preparation target", error))?;
        let has_url = request
            .url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty());
        if restart && !has_url && previous.is_none() {
            return Err(AppOperationError::NotFound(
                "Application to restart does not exist".into(),
            ));
        }
        if has_url
            && request
                .release_id
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            request.release_id = Some(super::start::generate_release_id());
        }
        let params = if has_url || request.env.is_some() || previous.is_none() {
            let mut env = if let Some(env) = &request.env {
                env.clone()
            } else if previous.is_some() {
                self.runtime
                    .get_app_container_spec(app_id)
                    .await
                    .map_err(|error| map_runtime_error("Read deployment environment", error))?
                    .env
                    .unwrap_or_default()
            } else {
                Default::default()
            };
            if has_url {
                crate::release_flow::identity::strip_release_identity(&mut env);
                crate::release_flow::identity::ensure_no_reserved_env(&env)?;
                env.insert(
                    "APP_DEPLOY_URL".into(),
                    request.url.clone().ok_or_else(|| {
                        AppOperationError::Validation("Deployment URL is required".into())
                    })?,
                );
                env.insert(
                    "APP_RELEASE_ID".into(),
                    request.release_id.clone().ok_or_else(|| {
                        AppOperationError::Validation("Release identity is required".into())
                    })?,
                );
                env.insert(
                    "APP_DEPLOY_SHA256".into(),
                    super::start::normalize_deploy_sha(request.sha256.as_deref())?,
                );
                env.insert(
                    shared_types::APP_DEPLOY_OPERATION_ID.into(),
                    operation_id.into(),
                );
                env.insert(
                    shared_types::APP_DEPLOY_GENERATION_ID.into(),
                    operation_id.into(),
                );
            } else {
                crate::release_flow::identity::ensure_business_env(&env)?;
            }
            Some(if let Some(current) = &previous {
                self.build_container_params_from_update(
                    app_id,
                    &UpdateAppRequest {
                        request_id: None,
                        lifecycle_id: request.lifecycle_id.clone(),
                        name: None,
                        image: None,
                        env: Some(env),
                        secrets: None,
                        resources: None,
                        tenant_id: None,
                        space_id: None,
                        expected_resource_version: current.resource_version.clone(),
                        recycle_enabled: request.idle_timeout_seconds.map(|value| value > 0),
                        idle_timeout_seconds: request.idle_timeout_seconds,
                    },
                    current,
                )
                .await?
            } else {
                let mut create = self.empty_runtime_request(
                    app_id,
                    app_id,
                    Some(env),
                    request.lifecycle_id.as_deref(),
                )?;
                create.recycle_enabled = request.idle_timeout_seconds.map(|value| value > 0);
                create.idle_timeout_seconds = request.idle_timeout_seconds;
                self.build_container_params(app_id, &create).await?
            })
        } else {
            None
        };
        Ok(DeployInput {
            version: 1,
            request,
            params,
            previous,
            restart,
        })
    }

    async fn validate_hot_runtime_configuration(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> AppResult<()> {
        let Some(captured) = self
            .runtime_configuration
            .operation_runtime_configuration(context)
            .await?
        else {
            return Ok(());
        };
        let status = self
            .runtime_configuration
            .runtime_configuration_status(&context.app_id, &context.lifecycle_id, captured.scope)
            .await?
            .ok_or_else(|| {
                AppOperationError::Backend("Captured runtime configuration has no status".into())
            })?;
        if status.applied_version != Some(captured.config_version) {
            return Err(AppOperationError::HotDeployEnvChange(
                "Captured runtime credentials require a cold deployment before they can take effect".into(),
            ));
        }
        Ok(())
    }

    pub(super) async fn execute_deploy_input(
        &self,
        app_id: &str,
        input: DeployInput,
        operation: &mut OwnedOperation,
        guard: Arc<AppOperationGuard>,
    ) -> AppResult<StartAppResult> {
        let DeployInput {
            request,
            mut params,
            previous,
            restart,
            ..
        } = input;
        operation.bind_lease(&guard).await?;
        let context = operation.execution_context();
        super::start::validate_start_request(app_id, &request)?;
        // StartDeployment can contain deploy_mode=hot, so checking only the
        // HotDeploy admission kind does not cover this public entrypoint.
        if request.deploy_mode == Some(DeployMode::Hot) {
            self.validate_hot_runtime_configuration(&context).await?;
        }
        let captured_configuration = self
            .runtime_configuration
            .operation_runtime_configuration(&context)
            .await?;
        if request.pg.is_some() && captured_configuration.is_none() {
            return Err(AppOperationError::InvalidState(
                "Deployment credentials require an admission configuration capture".into(),
            ));
        }
        if let Some(captured) = &captured_configuration
            && request.pg.as_ref().is_some_and(|pg| pg != &captured.pg)
        {
            return Err(AppOperationError::Validation("Request credentials differ from the captured configuration; save the desired configuration before starting the operation".into()));
        }
        let managed_cold =
            captured_configuration.is_some() && request.deploy_mode != Some(DeployMode::Hot);
        if managed_cold {
            if params.is_none() {
                let current = previous.as_ref().ok_or_else(|| {
                    AppOperationError::NotFound(
                        "Managed runtime replacement target is missing".into(),
                    )
                })?;
                params = Some(
                    self.build_container_params_from_update(
                        app_id,
                        &UpdateAppRequest {
                            request_id: None,
                            lifecycle_id: Some(context.lifecycle_id.clone()),
                            name: None,
                            image: None,
                            env: None,
                            secrets: None,
                            resources: None,
                            tenant_id: None,
                            space_id: None,
                            expected_resource_version: current.resource_version.clone(),
                            recycle_enabled: request.idle_timeout_seconds.map(|idle| idle > 0),
                            idle_timeout_seconds: request.idle_timeout_seconds,
                        },
                        current,
                    )
                    .await?,
                );
            }
            if let (Some(params), Some(captured)) =
                (params.as_mut(), captured_configuration.as_ref())
            {
                inject_captured_configuration(params, captured, &context, request.url.is_some());
            }
        }
        let url = request
            .url
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let mut hot = false;
        if let Some(url) = url
            && request.deploy_mode == Some(DeployMode::Hot)
        {
            operation
                .checkpoint("hot_preflight", serde_json::json!({"context":context}))
                .await?;
            hot = self
                .try_deploy_via_container_api_with_guard(
                    app_id,
                    super::hot_deploy::HotArtifact {
                        url,
                        release_id: request.release_id.as_deref().ok_or_else(|| {
                            AppOperationError::Validation("Release identity is required".into())
                        })?,
                        sha256: &super::start::normalize_deploy_sha(request.sha256.as_deref())?,
                    },
                    &request,
                    guard.clone(),
                    &context.operation_id,
                )
                .await?
                .is_some();
        }
        let configuration_applied = !hot && params.is_some();
        if captured_configuration.is_some() && request.deploy_mode == Some(DeployMode::Hot) {
            if !hot {
                return Err(AppOperationError::HotDeployEnvChange(
                    "Managed hot deployment is unavailable; explicitly request a cold deployment"
                        .into(),
                ));
            }
            let deadline_ms = self
                .metadata
                .store
                .operation_deadline(app_id, &context.operation_id)
                .await?
                .ok_or_else(|| {
                    AppOperationError::InvalidState(
                        "Managed hot deployment has no durable deadline".into(),
                    )
                })?;
            let remaining = deadline_ms
                .saturating_sub(chrono::Utc::now().timestamp_millis())
                .max(0) as u64;
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(remaining);
            tokio::time::timeout_at(
                deadline,
                self.observe_applied_runtime_configuration(&context, deadline),
            )
            .await
            .map_err(|_| {
                AppOperationError::Backend(
                    "Managed hot deployment observation exceeded the operation deadline".into(),
                )
            })??;
        }
        if !hot {
            if let Some(params) = params {
                if let Some(previous) = previous {
                    self.execute_update(app_id, params, previous, operation, &guard)
                        .await?;
                } else {
                    self.execute_creation(app_id, params, operation, &guard)
                        .await?;
                }
            } else {
                let target = self
                    .runtime
                    .capture_app_mutation_target(
                        &context,
                        previous
                            .as_ref()
                            .and_then(|value| value.resource_version.as_deref()),
                    )
                    .await
                    .map_err(|error| {
                        map_runtime_error("Capture deployment control target", error)
                    })?;
                operation
                    .checkpoint(
                        "deployment_control_target",
                        serde_json::json!({"target":target}),
                    )
                    .await?;
                guard.mark_mutating()?;
                if restart {
                    self.runtime.restart_app_target(&target).await
                } else {
                    self.runtime.start_app_target(&target).await
                }
                .map_err(|error| map_runtime_error("Start captured deployment", error))?;
            }
            if managed_cold {
                self.complete_cold_runtime_configuration(&context).await?;
            } else if url.is_some() {
                self.wait_deploy_stage(app_id, &context.operation_id, &guard)
                    .await?;
            }
        }
        // Hot and control-only paths do not rebuild configuration, so apply the
        // policy under the same operation and physical target fence.
        if let Some(idle) = request.idle_timeout_seconds
            && !configuration_applied
        {
            let target = self
                .runtime
                .capture_app_mutation_target(&context, None)
                .await
                .map_err(|error| map_runtime_error("Capture deployment policy target", error))?;
            operation
                .checkpoint(
                    "deployment_policy_target",
                    serde_json::json!({"target":target}),
                )
                .await?;
            guard.mark_mutating()?;
            self.runtime
                .patch_app_policy_target(
                    &target,
                    &shared_types::UserAppRuntimePolicy {
                        recycle_enabled: Some(idle > 0),
                        idle_timeout_seconds: Some(idle),
                        wake_on_traffic: Some(true),
                    },
                )
                .await
                .map_err(|error| map_runtime_error("Apply deployment policy", error))?;
        }
        let sql_report = if url.is_some() && !hot && request.auto_execute_sql.unwrap_or(true) {
            match self.execute_database_sql(app_id).await {
                Ok(report) => Some(report),
                Err(error) => {
                    tracing::warn!(app_id, %error, "Deployment SQL execution failed");
                    None
                }
            }
        } else {
            None
        };
        let pg_aligned = if captured_configuration.is_some() {
            Some(true)
        } else if request.pg.is_some() {
            return Err(AppOperationError::InvalidState(
                "Deployment credentials were not captured at admission".into(),
            ));
        } else {
            None
        };
        let pg_error = None;
        self.invalidate_deploy_cache().await;
        let mut runtime = self.get_app(app_id).await?;
        runtime.wake_on_traffic = Some(true);
        if let Some(idle) = request.idle_timeout_seconds {
            runtime.recycle_enabled = Some(idle > 0);
            runtime.idle_timeout_seconds = Some(idle);
        }
        let result = StartAppResult {
            operation_id: Some(context.operation_id),
            runtime,
            release_id: if url.is_some() {
                request.release_id
            } else {
                None
            },
            pg_aligned,
            pg_error,
            sql_report,
        };
        operation
            .checkpoint(
                "deployment_completed",
                serde_json::to_value(&result).map_err(|_| {
                    AppOperationError::Backend("Encode deployment completion".into())
                })?,
            )
            .await?;
        Ok(result)
    }
}

fn inject_captured_configuration(
    params: &mut container_runtime_api::ContainerCreateParams,
    captured: &shared_types::RuntimeConfigurationCapture,
    context: &shared_types::UserAppExecutionContext,
    has_artifact: bool,
) {
    let env = params.env.get_or_insert_with(Default::default);
    // The immutable generation's Secret wins over old manifest/image defaults.
    env.remove("POSTGRES_USER");
    env.remove("POSTGRES_PASSWORD");
    env.insert(
        shared_types::APP_RUNTIME_CONFIGURATION_VERSION.into(),
        captured.config_version.to_string(),
    );
    env.insert(
        shared_types::APP_DEPLOY_OPERATION_ID.into(),
        context.operation_id.clone(),
    );
    env.insert(
        shared_types::APP_DEPLOY_GENERATION_ID.into(),
        context.operation_id.clone(),
    );
    if !has_artifact {
        // Restart the confirmed workspace; do not replay an old download URL.
        for key in ["APP_DEPLOY_URL", "APP_RELEASE_ID", "APP_DEPLOY_SHA256"] {
            env.remove(key);
        }
    }
    let secrets = params.secrets.get_or_insert_with(Default::default);
    for key in [
        shared_types::APP_RUNTIME_CONFIGURATION_VERSION,
        shared_types::APP_DEPLOY_OPERATION_ID,
        shared_types::APP_DEPLOY_GENERATION_ID,
        "APP_DEPLOY_URL",
        "APP_RELEASE_ID",
        "APP_DEPLOY_SHA256",
    ] {
        secrets.remove(key);
    }
    let token = secrets
        .remove("APP_CLI_DEPLOY_TOKEN")
        .or_else(|| env.remove("APP_CLI_DEPLOY_TOKEN"))
        .filter(|token| !token.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    env.remove("APP_CLI_DEPLOY_TOKEN");
    // Existing hot-deploy and status clients read the platform token from the
    // generation env snapshot. Keep that contract while PG credentials live in Secret.
    env.insert("APP_CLI_DEPLOY_TOKEN".into(), token);
    secrets.insert("POSTGRES_USER".into(), captured.pg.username.clone());
    secrets.insert("POSTGRES_PASSWORD".into(), captured.pg.password.clone());
}

#[cfg(test)]
mod runtime_configuration_tests {
    use super::*;

    #[tokio::test]
    async fn cold_deployment_uses_captured_secret_and_activates_only_after_pg_verification() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let mut service = crate::test_support::test_service(root.path(), runtime.clone()).await;
        // This fixture asserts the runtime lease at every management call.
        // Docker uses a filesystem lease instead, so exercise the K8s lease
        // path explicitly rather than accidentally mixing both models.
        service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
        let app_id = "coldcredentials";
        let identity = service
            .metadata
            .store
            .ensure_identity(app_id)
            .await
            .unwrap();
        let save =
            |id: &str, revision, password: &str| shared_types::SaveRuntimeConfigurationRequest {
                lifecycle_id: identity.lifecycle_id.clone(),
                request_id: id.into(),
                expected_revision: revision,
                pg: StartPgCredential {
                    username: "business".into(),
                    password: password.into(),
                },
            };
        service
            .runtime_configuration
            .save_runtime_configuration(
                app_id,
                shared_types::UserAppOperationScope::Prod,
                &save("save1", 0, "capturedpassword"),
            )
            .await
            .unwrap();
        let guard = Arc::new(
            service
                .try_acquire_process_release_lock(app_id)
                .await
                .unwrap(),
        );
        let mut operation = OwnedOperation::admit(
            service.metadata.store.clone(),
            shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app_id.into(),
                lifecycle_id: Some(identity.lifecycle_id.clone()),
                operation_id: "coldoperation".into(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind: UserAppOperationKind::StartDeployment,
            },
        )
        .await
        .unwrap();
        let context = operation.execution_context();
        service
            .metadata
            .store
            .bind_operation_deadline(
                app_id,
                &context.operation_id,
                &context.lifecycle_id,
                chrono::Utc::now().timestamp_millis() + 30_000,
            )
            .await
            .unwrap();
        // A later save cannot change the version already owned by this operation.
        service
            .runtime_configuration
            .save_runtime_configuration(
                app_id,
                shared_types::UserAppOperationScope::Prod,
                &save("save2", 1, "pendingpassword"),
            )
            .await
            .unwrap();
        *runtime.configuration_replies.lock().unwrap() = [
            (0, "initialadmin\n"),
            (0, "1\n"),
            (2, ""),
            (0, "1\n"),
            (0, "ALTER ROLE"),
            (0, "1\n"),
            (0, r#"{"success":true,"data":{"activated":true}}"#),
            (0, r#"{"status":"ready","phase":"running"}"#),
        ]
        .into_iter()
        .map(|(exit_code, stdout)| container_runtime_api::ExecResult {
            exit_code,
            stdout: stdout.into(),
            stderr: String::new(),
        })
        .collect();
        let mut params = container_runtime_api::ContainerCreateParams::builder()
            .project_id(app_id)
            .service_type(shared_types::ServiceType::Userapp)
            .build();
        params.env = Some(std::collections::HashMap::from([
            ("POSTGRES_USER".into(), "olduser".into()),
            ("POSTGRES_PASSWORD".into(), "oldpassword".into()),
            ("APP_DEPLOY_URL".into(), "http://obsolete/artifact".into()),
        ]));
        let result = service
            .execute_deploy_input(
                app_id,
                DeployInput {
                    version: 1,
                    request: StartAppRequest::default(),
                    params: Some(params),
                    previous: None,
                    restart: false,
                },
                &mut operation,
                guard.clone(),
            )
            .await
            .unwrap();
        assert_eq!(result.pg_aligned, Some(true));
        let history = runtime.create_params_history.get(app_id).unwrap();
        let actual = &history[0];
        assert!(
            !actual
                .env
                .as_ref()
                .unwrap()
                .contains_key("POSTGRES_PASSWORD")
        );
        assert!(!actual.env.as_ref().unwrap().contains_key("APP_DEPLOY_URL"));
        assert_eq!(
            actual
                .secrets
                .as_ref()
                .unwrap()
                .get("POSTGRES_PASSWORD")
                .unwrap(),
            "capturedpassword"
        );
        let applied_environment = actual.env.clone();
        drop(history);
        {
            let commands = runtime.configuration_commands.lock().unwrap();
            let scripts: Vec<_> = commands
                .iter()
                .map(|command| command.last().unwrap().as_str())
                .collect();
            assert!(scripts[4].contains("ALTER USER"));
            assert!(scripts[5].starts_with("env -u PGHOSTADDR -u PGSERVICE PGPASSWORD="));
            assert!(scripts[6].contains("/configuration/activate"));
            assert!(
                !scripts
                    .iter()
                    .any(|script| script.contains("pendingpassword"))
            );
        }
        assert!(runtime.configuration_replies.lock().unwrap().is_empty());
        operation.succeed().await.unwrap();
        guard.mark_completed();
        Arc::try_unwrap(guard).ok().unwrap().finish().await.unwrap();
        let status = service
            .runtime_configuration
            .runtime_configuration_status(
                app_id,
                &identity.lifecycle_id,
                shared_types::UserAppOperationScope::Prod,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.applied_version, Some(1));
        assert_eq!(status.saved_version, 2);
        assert!(status.pending);

        // Traffic is a new operation against the existing deployment generation.
        // It verifies version 1 without applying the subsequently saved version 2.
        runtime.specs.insert(
            app_id.into(),
            container_runtime_api::ContainerSpecSnapshot {
                env: applied_environment,
                ..Default::default()
            },
        );
        runtime.configuration_commands.lock().unwrap().clear();
        *runtime.configuration_replies.lock().unwrap() =
            ["1\n", r#"{"status":"ready","phase":"running"}"#]
                .into_iter()
                .map(|stdout| container_runtime_api::ExecResult {
                    exit_code: 0,
                    stdout: stdout.into(),
                    stderr: String::new(),
                })
                .collect();
        assert!(matches!(
            service
                .wake_app_on_traffic(app_id, std::time::Duration::from_secs(10))
                .await
                .unwrap(),
            shared_types::WakeOutcome::AlreadyRunning
        ));
        {
            let commands = runtime.configuration_commands.lock().unwrap();
            assert_eq!(commands.len(), 2);
            let targets = runtime.configuration_targets.lock().unwrap();
            assert_eq!(targets.len(), 2);
            assert_eq!(
                targets[0], targets[1],
                "wake must use the existing generation"
            );

            let verify = commands[0].last().unwrap();
            assert!(verify.contains("capturedpassword"));
            assert!(!verify.contains("pendingpassword"));
            assert!(commands.iter().all(|command| {
                let script = command.last().unwrap();
                !script.contains("ALTER USER") && !script.contains("/configuration/activate")
            }));
        }
        let status = service
            .runtime_configuration
            .runtime_configuration_status(
                app_id,
                &identity.lifecycle_id,
                shared_types::UserAppOperationScope::Prod,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.applied_version, Some(1));
        assert_eq!(status.saved_version, 2);
        assert!(status.pending);
    }

    #[tokio::test]
    async fn start_deployment_hot_gate_uses_admission_capture() {
        let root = tempfile::tempdir().unwrap();
        let service = crate::test_support::test_service(
            root.path(),
            Arc::new(crate::test_support::MockRuntime::default()),
        )
        .await;
        let app_id = "hotcredentials";
        let identity = service
            .metadata
            .store
            .ensure_identity(app_id)
            .await
            .unwrap();
        let scope = shared_types::UserAppOperationScope::Prod;
        let save = |revision, request: &str, password: &str| {
            shared_types::SaveRuntimeConfigurationRequest {
                lifecycle_id: identity.lifecycle_id.clone(),
                request_id: request.into(),
                expected_revision: revision,
                pg: StartPgCredential {
                    username: "business".into(),
                    password: password.into(),
                },
            }
        };
        service
            .runtime_configuration
            .save_runtime_configuration(app_id, scope, &save(0, "save1", "password1"))
            .await
            .unwrap();
        let operation = OwnedOperation::admit(
            service.metadata.store.clone(),
            shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app_id.into(),
                lifecycle_id: Some(identity.lifecycle_id.clone()),
                operation_id: "hotoperation".into(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind: UserAppOperationKind::StartDeployment,
            },
        )
        .await
        .unwrap();
        let context = operation.execution_context();
        service
            .runtime_configuration
            .save_runtime_configuration(app_id, scope, &save(1, "save2", "password2"))
            .await
            .unwrap();
        let capture = service
            .runtime_configuration
            .operation_runtime_configuration(&context)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(capture.config_version, 1);
        assert_eq!(capture.pg.password, "password1");
        let error = service
            .validate_hot_runtime_configuration(&context)
            .await
            .unwrap_err();
        assert!(matches!(error, AppOperationError::HotDeployEnvChange(_)));
        let after = service
            .runtime_configuration
            .operation_runtime_configuration(&context)
            .await
            .unwrap()
            .unwrap();
        assert!(after.target.is_none());
        assert_eq!(
            after.credentials,
            shared_types::CredentialApplicationState::Captured
        );
        operation.reject_without_mutation(&error).await.unwrap();
    }
}
