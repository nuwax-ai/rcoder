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
        user_id: String::new(),
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
            .validate_request_lifecycle(app_id, &request.user_id, request.lifecycle_id.as_deref())
            .await?;
        let identity = self
            .metadata
            .store
            .ensure_identity(app_id)
            .await?;
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
        let mut operation = OwnedOperation::admit_with_input(
            self.metadata.store.clone(),
            shared_types::UserAppAdmission {
                runtime_policy_on_success: Some(policy),
                metadata: input
                    .previous
                    .is_none()
                    .then(|| shared_types::UserAppMetadataPatch {
                        app_id: app_id.into(),
                        user_id: identity.user_id.clone(),
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
        )
        .await?;
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
                        user_id: String::new(),
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
                    &request.user_id,
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

    pub(super) async fn execute_deploy_input(
        &self,
        app_id: &str,
        input: DeployInput,
        operation: &mut OwnedOperation,
        guard: Arc<AppOperationGuard>,
    ) -> AppResult<StartAppResult> {
        let DeployInput {
            request,
            params,
            previous,
            restart,
            ..
        } = input;
        operation.bind_lease(&guard, &request.user_id).await?;
        let context = operation.execution_context(&request.user_id);
        super::start::validate_start_request(app_id, &request)?;
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
        if !hot {
            if let Some(params) = params {
                if let Some(previous) = previous {
                    self.execute_update(
                        app_id,
                        &request.user_id,
                        params,
                        previous,
                        operation,
                        &guard,
                    )
                    .await?;
                } else {
                    self.execute_creation(app_id, &request.user_id, params, operation, &guard)
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
            if url.is_some() {
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
        let (pg_aligned, pg_error) = match &request.pg {
            Some(credentials) => match self
                .align_start_pg(app_id, &request.user_id, credentials)
                .await
            {
                Ok(()) => (Some(true), None),
                Err(error) => {
                    let message = error.to_string();
                    let message = if credentials.password.is_empty() {
                        message
                    } else {
                        message.replace(&credentials.password, "[REDACTED]")
                    };
                    (Some(false), Some(message))
                }
            },
            None => (None, None),
        };
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
