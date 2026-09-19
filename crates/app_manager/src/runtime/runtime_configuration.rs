//! One captured configuration version and physical target own credential writes.
//! No database transaction remains open while the runtime executes commands.
use async_trait::async_trait;
use shared_types::{BusinessStartupState, CredentialApplicationState, CredentialMutationEvidence};

use crate::{models::*, service::AppService};

struct BoundManagementRunner<'a> {
    service: &'a AppService,
    context: &'a shared_types::UserAppExecutionContext,
    target: &'a shared_types::RuntimeConfigurationTarget,
}

#[async_trait]
impl shared_types::PgCommandRunner for BoundManagementRunner<'_> {
    async fn run(&self, command: &str) -> Result<shared_types::CommandOutcome, String> {
        let result = self
            .service
            .runtime
            .exec_app_configuration_target(
                self.context,
                self.target,
                vec!["sh".into(), "-c".into(), command.into()],
            )
            .await
            .map_err(|error| format!("Bound PostgreSQL command failed: {error}"))?;
        Ok(shared_types::CommandOutcome {
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
        })
    }
}

impl AppService {
    /// Wake/hot deployment may verify only the previously applied version. This
    /// path never changes PG passwords or opens a different generation's gate.
    pub(crate) async fn observe_applied_runtime_configuration(
        &self,
        context: &shared_types::UserAppExecutionContext,
        deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        use shared_types::PgCommandRunner as _;
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
                AppOperationError::InvalidState("Captured runtime configuration has no head".into())
            })?;
        if status.applied_version != Some(captured.config_version) {
            return Err(AppOperationError::InvalidState(
                "Automatic wake and hot deployment cannot promote pending credentials".into(),
            ));
        }
        let spec = self
            .runtime
            .get_app_container_spec(&context.app_id)
            .await
            .map_err(|error| {
                crate::utils::map_runtime_error("Read active configuration identity", error)
            })?;
        let env = spec.env.as_ref().ok_or_else(|| {
            AppOperationError::InvalidState(
                "Active runtime configuration environment is missing".into(),
            )
        })?;
        let generation = env
            .get(shared_types::APP_DEPLOY_GENERATION_ID)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AppOperationError::InvalidState("Active runtime generation is missing".into())
            })?;
        if env
            .get(shared_types::APP_RUNTIME_CONFIGURATION_VERSION)
            .and_then(|value| value.parse::<i64>().ok())
            != Some(captured.config_version)
        {
            return Err(AppOperationError::InvalidState(
                "Active container configuration differs from the applied version".into(),
            ));
        }
        let target = self
            .runtime
            .capture_app_configuration_target(context, generation)
            .await
            .map_err(|error| {
                crate::utils::map_runtime_error("Capture active configuration target", error)
            })?;
        self.runtime_configuration
            .bind_runtime_configuration_target(context, captured.config_version, &target)
            .await?;
        self.runtime_configuration
            .record_runtime_configuration_result(
                context,
                captured.config_version,
                &target,
                CredentialApplicationState::Applying,
                BusinessStartupState::NotStarted,
            )
            .await
            .map_err(configuration_commit_unknown)?;
        let runner = BoundManagementRunner {
            service: self,
            context,
            target: &target,
        };
        let verification = runner
            .run(&shared_types::pg_utils::pg_verify_credentials_cmd(
                &captured.pg.username,
                &captured.pg.password,
            ))
            .await;
        if !matches!(verification, Ok(ref result) if result.exit_code == 0) {
            self.runtime_configuration
                .record_runtime_configuration_result(
                    context,
                    captured.config_version,
                    &target,
                    CredentialApplicationState::Failed,
                    BusinessStartupState::NotStarted,
                )
                .await
                .map_err(configuration_commit_unknown)?;
            return Err(AppOperationError::CredentialApplication {
                message: "Applied runtime credentials could not be verified; automatic recovery will not change the password".into(),
                mutation: CredentialMutationEvidence::NotAttempted,
            });
        }
        self.runtime_configuration
            .record_runtime_configuration_result(
                context,
                captured.config_version,
                &target,
                CredentialApplicationState::Applied,
                BusinessStartupState::Starting,
            )
            .await
            .map_err(configuration_commit_unknown)?;
        self.wait_configuration_business(context, &target, captured.config_version, deadline)
            .await
    }

    pub(crate) async fn complete_cold_runtime_configuration(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> AppResult<()> {
        let deadline_ms = self
            .metadata
            .store
            .operation_deadline(&context.app_id, &context.operation_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::InvalidState(
                    "Managed configuration operation has no durable deadline".into(),
                )
            })?;
        let remaining = deadline_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis())
            .max(0) as u64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(remaining);
        let captured = self
            .runtime_configuration
            .operation_runtime_configuration(context)
            .await?
            .ok_or_else(|| {
                AppOperationError::InvalidState("Managed configuration capture is missing".into())
            })?;
        let target = if let Some(target) = captured.target.clone() {
            target
        } else {
            loop {
                let capture = self
                    .runtime
                    .capture_app_configuration_target(context, &context.operation_id);
                match tokio::time::timeout_at(deadline, capture).await {
                    Ok(Ok(target)) => break target,
                    Ok(Err(container_runtime_api::ContainerRuntimeError::ConfigurationError(message))) => return Err(AppOperationError::Validation(message)),
                    _ if tokio::time::Instant::now() < deadline => tokio::time::sleep_until(std::cmp::min(deadline,
                        tokio::time::Instant::now() + std::time::Duration::from_millis(500))).await,
                    _ => return Err(AppOperationError::Backend("Captured configuration management target did not become available within the operation budget".into())),
                }
            }
        };
        self.runtime_configuration
            .bind_runtime_configuration_target(context, captured.config_version, &target)
            .await?;
        let activation = async {
            let admin = self
                .wait_for_configuration_postgres(context, &target, deadline)
                .await?;
            self.apply_captured_runtime_credentials(context, &target, &admin)
                .await?;
            self.activate_captured_runtime_configuration(context, &target)
                .await
        };
        tokio::time::timeout_at(deadline, activation).await.map_err(|_| AppOperationError::CredentialApplication {
            message: "Configuration activation exceeded the operation deadline; command completion requires reconciliation".into(),
            mutation: CredentialMutationEvidence::Unknown,
        })??;
        self.wait_configuration_business(context, &target, captured.config_version, deadline)
            .await
    }

    async fn wait_configuration_business(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        version: i64,
        deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        use shared_types::PgCommandRunner as _;
        let runner = BoundManagementRunner {
            service: self,
            context,
            target,
        };
        loop {
            // Success requires Running rather than Idle's infrastructure readiness.
            let observation = runner.run("curl --silent --show-error --fail --connect-timeout 5 --max-time 10 --noproxy '*' http://127.0.0.1:3010/ready");
            if let Ok(Ok(reply)) = tokio::time::timeout_at(deadline, observation).await
                && reply.exit_code == 0
                && serde_json::from_str::<serde_json::Value>(&reply.stdout).is_ok_and(|value| {
                    value.get("status").and_then(serde_json::Value::as_str) == Some("ready")
                        && value.get("phase").and_then(serde_json::Value::as_str)
                            == Some(shared_types::AppCliDeployPhase::Running.as_str())
                })
            {
                self.runtime_configuration
                    .record_runtime_configuration_result(
                        context,
                        version,
                        target,
                        CredentialApplicationState::Applied,
                        BusinessStartupState::Ready,
                    )
                    .await
                    .map_err(configuration_commit_unknown)?;
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                self.runtime_configuration
                    .record_runtime_configuration_result(
                        context,
                        version,
                        target,
                        CredentialApplicationState::Applied,
                        BusinessStartupState::Unknown,
                    )
                    .await
                    .map_err(configuration_commit_unknown)?;
                return Err(AppOperationError::CredentialApplication {
                    message: "Credentials are applied but business readiness was not confirmed within the operation budget".into(),
                    mutation: CredentialMutationEvidence::AppliedButUnverified,
                });
            }
            let failure = runner.run("curl --silent --show-error --fail --connect-timeout 5 --max-time 10 --noproxy '*' http://127.0.0.1:3010/v1/deploy/status");
            if let Ok(Ok(reply)) = tokio::time::timeout_at(deadline, failure).await
                && reply.exit_code == 0
                && serde_json::from_str::<serde_json::Value>(&reply.stdout).is_ok_and(|value| {
                    value
                        .get("data")
                        .and_then(|data| data.get("phase"))
                        .and_then(serde_json::Value::as_str)
                        == Some(shared_types::AppCliDeployPhase::Failed.as_str())
                })
            {
                self.runtime_configuration
                    .record_runtime_configuration_result(
                        context,
                        version,
                        target,
                        CredentialApplicationState::Applied,
                        BusinessStartupState::Failed,
                    )
                    .await
                    .map_err(configuration_commit_unknown)?;
                return Err(AppOperationError::Backend(format!(
                    "Business startup failed for applied configuration {} (operation {}); runtime cleanup requires reconciliation",
                    version, context.operation_id
                )));
            }
            tokio::time::sleep_until(std::cmp::min(
                deadline,
                tokio::time::Instant::now() + std::time::Duration::from_millis(500),
            ))
            .await;
        }
    }

    /// This phase waits for the PG management socket, never business readiness.
    /// The caller supplies its overall operation deadline; retries cannot extend it.
    pub(crate) async fn wait_for_configuration_postgres(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        deadline: tokio::time::Instant,
    ) -> AppResult<shared_types::pg_utils::PgAdministrationTarget> {
        use shared_types::PgCommandRunner as _;
        let runner = BoundManagementRunner {
            service: self,
            context,
            target,
        };
        loop {
            let attempt = async {
                // The image writes this marker from initdb/verified legacy PGDATA.
                // Never guess the administrator from a new business credential.
                let Ok(marker) = runner
                    .run("test -n \"${PGDATA:-}\" && cat \"$PGDATA/.rcoder-admin-user\"")
                    .await
                else {
                    return Ok::<_, AppOperationError>(None);
                };
                if marker.exit_code != 0 {
                    return Ok(None);
                }
                let admin = shared_types::pg_utils::PgAdministrationTarget::new(
                    marker.stdout.trim().into(),
                    "/var/run/postgresql".into(),
                )
                .map_err(|_| AppOperationError::InvalidState("PGDATA administrator identity is invalid; explicit reconciliation is required".into()))?;
                let Ok(ready) = runner.run(&admin.business_database_ready_command()).await else {
                    return Ok(None);
                };
                Ok((ready.exit_code == 0 && ready.stdout.trim() == "1").then_some(admin))
            };
            match tokio::time::timeout_at(deadline, attempt).await {
                Ok(Ok(Some(admin))) => return Ok(admin),
                Ok(Err(error)) => return Err(error),
                Ok(Ok(None)) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep_until(std::cmp::min(deadline, tokio::time::Instant::now() + std::time::Duration::from_millis(500))).await;
                }
                _ => return Err(AppOperationError::CredentialApplication {
                    message: "PostgreSQL management readiness was not confirmed within the operation budget".into(),
                    mutation: CredentialMutationEvidence::NotAttempted,
                }),
            }
        }
    }

    /// Open only the startup gate associated with an already committed PG result.
    /// The request goes through the captured management container, not a Service
    /// whose readiness depends on the business that is still waiting to start.
    pub(crate) async fn activate_captured_runtime_configuration(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
    ) -> AppResult<()> {
        use shared_types::PgCommandRunner as _;
        let captured = self
            .runtime_configuration
            .operation_runtime_configuration(context)
            .await?
            .ok_or_else(|| {
                AppOperationError::InvalidState("Runtime configuration capture is missing".into())
            })?;
        if captured.credentials != CredentialApplicationState::Applied
            || captured.target.as_ref() != Some(target)
        {
            return Err(AppOperationError::InvalidState("Only confirmed credentials on the captured physical target can activate business startup".into()));
        }
        let activation = shared_types::RuntimeConfigurationActivation {
            operation_id: context.operation_id.clone(),
            deployment_generation: target.deployment_generation.clone(),
            config_version: captured.config_version,
        };
        let body = serde_json::to_string(&activation).map_err(|_| {
            AppOperationError::Backend("Encode configuration activation identity".into())
        })?;
        self.runtime_configuration
            .record_runtime_configuration_result(
                context,
                captured.config_version,
                target,
                CredentialApplicationState::Applied,
                BusinessStartupState::Starting,
            )
            .await
            .map_err(configuration_commit_unknown)?;
        let runner = BoundManagementRunner {
            service: self,
            context,
            target,
        };
        // Token is expanded only inside the bound container; it never enters the
        // controller command string or durable operation checkpoint.
        let command = format!(
            "curl --silent --show-error --fail --connect-timeout 5 --max-time 15 --noproxy '*' -X POST -H 'content-type: application/json' -H \"x-deploy-token: $APP_CLI_DEPLOY_TOKEN\" --data {} http://127.0.0.1:3010/v1/runtime/configuration/activate",
            shared_types::pg_utils::pg_shell_quote(&body),
        );
        let confirmed = match runner.run(&command).await {
            Ok(result) if result.exit_code == 0 => {
                serde_json::from_str::<serde_json::Value>(&result.stdout).is_ok_and(|body| {
                    body.get("success").and_then(serde_json::Value::as_bool) == Some(true)
                        && body
                            .get("data")
                            .and_then(|data| data.get("activated"))
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                })
            }
            _ => false,
        };
        if !confirmed {
            self.runtime_configuration
                .record_runtime_configuration_result(
                    context,
                    captured.config_version,
                    target,
                    CredentialApplicationState::Applied,
                    BusinessStartupState::Unknown,
                )
                .await
                .map_err(configuration_commit_unknown)?;
            return Err(AppOperationError::CredentialApplication {
                message: "Credentials are applied but business activation acknowledgement is unknown; reconcile the original operation".into(),
                mutation: CredentialMutationEvidence::AppliedButUnverified,
            });
        }
        Ok(())
    }

    /// Caller retains the operation lease and confirms old business processes
    /// stopped before invoking this method. `admin` is the verified PGDATA
    /// initialization identity, never the pending business username.
    pub(crate) async fn apply_captured_runtime_credentials(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        admin: &shared_types::pg_utils::PgAdministrationTarget,
    ) -> AppResult<Option<StartPgCredential>> {
        let Some(captured) = self
            .runtime_configuration
            .operation_runtime_configuration(context)
            .await?
        else {
            return Ok(None);
        };
        if matches!(
            captured.credentials,
            CredentialApplicationState::Applying | CredentialApplicationState::Unknown
        ) {
            return Err(AppOperationError::CredentialApplication {
                message: "Previous credential application requires reconciliation under the original operation".into(),
                mutation: CredentialMutationEvidence::Unknown,
            });
        }
        self.runtime_configuration
            .bind_runtime_configuration_target(context, captured.config_version, target)
            .await?;
        if captured.credentials == CredentialApplicationState::Applied {
            return Ok(Some(captured.pg));
        }
        if captured.credentials != CredentialApplicationState::Captured {
            return Err(AppOperationError::InvalidState(
                "Credential application cannot restart a failed receipt".into(),
            ));
        }
        self.runtime_configuration
            .record_runtime_configuration_result(
                context,
                captured.config_version,
                target,
                CredentialApplicationState::Applying,
                BusinessStartupState::NotStarted,
            )
            .await
            .map_err(configuration_commit_unknown)?;
        let runner = BoundManagementRunner {
            service: self,
            context,
            target,
        };
        match shared_types::align_pg_credentials_with_admin(
            &runner,
            admin,
            &captured.pg.username,
            &captured.pg.password,
        )
        .await
        {
            Ok(_) => {
                self.runtime_configuration
                    .record_runtime_configuration_result(
                        context,
                        captured.config_version,
                        target,
                        CredentialApplicationState::Applied,
                        BusinessStartupState::NotStarted,
                    )
                    .await
                    .map_err(configuration_commit_unknown)?;
                Ok(Some(captured.pg))
            }
            Err(error) => {
                let state = match error.mutation_evidence() {
                    CredentialMutationEvidence::NotAttempted => CredentialApplicationState::Failed,
                    CredentialMutationEvidence::Unknown
                    | CredentialMutationEvidence::AppliedButUnverified => {
                        CredentialApplicationState::Unknown
                    }
                };
                // A failed persistence attempt leaves Applying, preserving the
                // original target and recovery protection rather than claiming failure.
                self.runtime_configuration
                    .record_runtime_configuration_result(
                        context,
                        captured.config_version,
                        target,
                        state,
                        BusinessStartupState::NotStarted,
                    )
                    .await
                    .map_err(configuration_commit_unknown)?;
                Err(AppOperationError::credential_failure(
                    error,
                    &captured.pg.password,
                ))
            }
        }
    }
}

fn configuration_commit_unknown(error: shared_types::UserAppStoreError) -> AppOperationError {
    AppOperationError::CredentialApplication {
        message: format!("Credential application state could not be confirmed: {error}"),
        mutation: CredentialMutationEvidence::Unknown,
    }
}
