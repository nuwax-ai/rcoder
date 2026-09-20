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
    /// Freeze the source owner before any physical replacement. A seal is itself
    /// a durable mutation: an unknown reply must retain this operation's fence.
    pub(crate) async fn seal_runtime_generation_source(
        &self,
        context: &shared_types::UserAppExecutionContext,
        authorization: &shared_types::RuntimeGenerationHandoff,
        operation: &mut crate::service::OwnedOperation,
        checkpoint: &mut serde_json::Value,
        guard: &crate::service::AppOperationGuard,
    ) -> AppResult<shared_types::RuntimeGenerationSourceSeal> {
        use shared_types::PgCommandRunner as _;
        let previous_mutation = guard.has_unfinished_mutation();
        let deadline_ms = self
            .metadata
            .store
            .operation_deadline(&context.app_id, &context.operation_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::InvalidState(
                    "Source sealing requires the original operation deadline".into(),
                )
            })?;
        let remaining = deadline_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis())
            .max(0) as u64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(remaining);
        let workload = tokio::time::timeout_at(
            deadline,
            self.runtime.capture_app_mutation_target(context, None),
        )
        .await
        .map_err(|_| {
            AppOperationError::Conflict("Source workload capture deadline exceeded".into())
        })?
        .map_err(|error| {
            crate::utils::map_runtime_error("Capture source workload identity", error)
        })?;
        if workload.resource.uid != authorization.previous_resource_uid
            || workload.resource.name != authorization.previous_resource_name
        {
            return Err(AppOperationError::Conflict(
                "Source workload identity changed before sealing".into(),
            ));
        }
        let captured = tokio::time::timeout_at(
            deadline,
            self.runtime
                .capture_app_configuration_target(context, &authorization.previous_generation),
        )
        .await
        .map_err(|_| {
            AppOperationError::Conflict("Source management capture deadline exceeded".into())
        })?;
        let target = match captured {
            Err(container_runtime_api::ContainerRuntimeError::ManagementNotRunning) => {
                guard.mark_mutating()?;
                return tokio::time::timeout_at(
                    deadline,
                    self.seal_stopped_runtime_source(
                        &workload,
                        authorization,
                        operation,
                        checkpoint,
                    ),
                )
                .await
                .map_err(|_| {
                    AppOperationError::InvalidState(
                        "Offline source seal outcome unknown; recover the original helper".into(),
                    )
                })?;
            }
            Ok(target) => {
                if checkpoint.get("offline_source_helper").is_some() {
                    guard.mark_mutating()?;
                    return Err(AppOperationError::Conflict("Source became active during offline handoff; original helper remains protected".into()));
                }
                target
            }
            Err(error) => {
                return Err(crate::utils::map_runtime_error(
                    "Capture source management identity before replacement",
                    error,
                ));
            }
        };
        if target.deployment_generation != authorization.previous_generation {
            return Err(AppOperationError::Conflict(
                "Source physical identity changed before sealing".into(),
            ));
        }
        // K8s workload UID and management Pod UID are different identities.
        // Persist both before the bound exec; retries cannot silently select a new Pod.
        if let Some(original) = checkpoint.get("source_management_target") {
            let original: shared_types::RuntimeConfigurationTarget =
                serde_json::from_value(original.clone()).map_err(|_| {
                    AppOperationError::InvalidState(
                        "Stored source management identity is invalid".into(),
                    )
                })?;
            if original != target {
                return Err(AppOperationError::Conflict(
                    "Original source management identity changed; handoff remains protected".into(),
                ));
            }
        }
        checkpoint["source_management_target"] =
            serde_json::to_value(&target).map_err(|error| {
                AppOperationError::Backend(format!("Encode source management identity: {error}"))
            })?;
        operation
            .checkpoint("source_seal_requested", checkpoint.clone())
            .await?;
        let body = serde_json::to_string(authorization).map_err(|_| {
            AppOperationError::InvalidState("Invalid source handoff authorization".into())
        })?;
        let command = format!(
            "curl --silent --show-error --connect-timeout 3 --max-time 5 --noproxy '*' -H \"x-deploy-token: $APP_CLI_DEPLOY_TOKEN\" -H 'content-type: application/json' --data-binary {} http://127.0.0.1:3010/v1/runtime/configuration/source-seal",
            shared_types::pg_utils::pg_shell_quote(&body)
        );
        let runner = BoundManagementRunner {
            service: self,
            context,
            target: &target,
        };
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AppOperationError::Conflict("Source owner is still preparing; physical runtime and credentials were not changed".into()));
            }
            guard.mark_mutating()?;
            let result = tokio::time::timeout_at(deadline, runner.run(&command)).await;
            let outcome = match result {
                Ok(Ok(reply)) if reply.exit_code == 0 => {
                    source_seal_reply(&reply.stdout, authorization)?
                }
                _ => {
                    return Err(AppOperationError::InvalidState(
                        "Source seal result is unknown; original operation remains protected"
                            .into(),
                    ));
                }
            };
            match outcome {
                Some(seal) => return Ok(seal),
                None => {
                    // Exact no-admission response. No seal or runtime mutation occurred.
                    if !previous_mutation {
                        guard.mark_rejected_before_mutation();
                    }
                    tokio::time::sleep_until(
                        (tokio::time::Instant::now() + std::time::Duration::from_millis(200))
                            .min(deadline),
                    )
                    .await;
                }
            }
        }
    }
    async fn seal_stopped_runtime_source(
        &self,
        source: &shared_types::UserAppMutationTarget,
        authorization: &shared_types::RuntimeGenerationHandoff,
        operation: &mut crate::service::OwnedOperation,
        checkpoint: &mut serde_json::Value,
    ) -> AppResult<shared_types::RuntimeGenerationSourceSeal> {
        operation
            .checkpoint("offline_source_seal_requested", checkpoint.clone())
            .await?;
        let helper: container_runtime_api::OfflineSourceSealTarget = match checkpoint
            .get("offline_source_helper")
        {
            Some(value) => serde_json::from_value(value.clone()).map_err(|_| {
                AppOperationError::InvalidState("Stored offline helper identity is invalid".into())
            })?,
            None => {
                let helper = self
                    .runtime
                    .prepare_stopped_source_seal(source, authorization)
                    .await
                    .map_err(|error| {
                        crate::utils::map_runtime_error("Prepare offline source helper", error)
                    })?;
                checkpoint["offline_source_helper"] =
                    serde_json::to_value(&helper).map_err(|_| {
                        AppOperationError::InvalidState(
                            "Cannot encode offline helper identity".into(),
                        )
                    })?;
                operation
                    .checkpoint("offline_source_helper_prepared", checkpoint.clone())
                    .await?;
                helper
            }
        };
        if helper.authorization != *authorization
            || helper.source.context != source.context
            || helper.source.resource.uid != source.resource.uid
        {
            return Err(AppOperationError::Conflict(
                "Original offline helper source identity mismatch".into(),
            ));
        }
        let seal: shared_types::RuntimeGenerationSourceSeal =
            if let Some(value) = checkpoint.get("source_seal") {
                serde_json::from_value(value.clone()).map_err(|_| {
                    AppOperationError::InvalidState("Stored offline seal receipt is invalid".into())
                })?
            } else {
                let seal = self
                    .runtime
                    .run_stopped_source_seal(&helper)
                    .await
                    .map_err(|error| {
                        crate::utils::map_runtime_error("Run original offline source helper", error)
                    })?;
                if seal.authorization != *authorization {
                    return Err(AppOperationError::InvalidState(
                        "Offline source seal identity mismatch".into(),
                    ));
                }
                checkpoint["source_seal"] = serde_json::to_value(&seal).map_err(|_| {
                    AppOperationError::InvalidState("Cannot encode offline source seal".into())
                })?;
                operation
                    .checkpoint("source_sealed", checkpoint.clone())
                    .await?;
                seal
            };
        if seal.authorization != *authorization {
            return Err(AppOperationError::InvalidState(
                "Stored offline source seal identity mismatch".into(),
            ));
        }
        self.runtime
            .cleanup_stopped_source_seal(&helper)
            .await
            .map_err(|error| {
                crate::utils::map_runtime_error("Clean original offline helper", error)
            })?;
        operation
            .checkpoint("offline_source_helper_cleaned", checkpoint.clone())
            .await?;
        Ok(seal)
    }

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
            let already_activated = self
                .verify_generation_handoff(context, &target, captured.config_version, deadline)
                .await?;
            if already_activated {
                if captured.credentials != CredentialApplicationState::Applied {
                    return Err(AppOperationError::InvalidState(
                        "Durable activation conflicts with captured credential evidence".into(),
                    ));
                }
                // Same original operation and physical target, after an owner
                // restart: only observe readiness. Never repeat credential writes
                // or issue another startup request using consumed authorization.
                return Ok(());
            }
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

    /// Read-only barrier before any credential command. Its expected value comes
    /// from the original durable update checkpoint, never from container env.
    async fn verify_generation_handoff(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        config_version: i64,
        deadline: tokio::time::Instant,
    ) -> AppResult<bool> {
        use shared_types::PgCommandRunner as _;
        let operation = self
            .metadata
            .store
            .get_operation(&context.app_id, &context.operation_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::InvalidState("Configuration operation is missing".into())
            })?;
        let Some(value) = operation.checkpoint.get("generation_handoff") else {
            return Ok(false);
        };
        let expected: shared_types::RuntimeGenerationHandoff =
            serde_json::from_value(value.clone()).map_err(|_| {
                AppOperationError::InvalidState("Generation handoff checkpoint is invalid".into())
            })?;
        expected.validate().map_err(AppOperationError::Validation)?;
        if expected.app_id != context.app_id
            || expected.lifecycle_id != context.lifecycle_id
            || expected.activation.operation_id != context.operation_id
            || expected.activation.deployment_generation != target.deployment_generation
            || expected.activation.config_version != config_version
        {
            return Err(AppOperationError::InvalidState(
                "Generation handoff checkpoint identity mismatch".into(),
            ));
        }
        let source_seal: shared_types::RuntimeGenerationSourceSeal = operation
            .checkpoint
            .get("source_seal")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .ok_or_else(|| {
                AppOperationError::InvalidState(
                    "Generation handoff has no valid source seal".into(),
                )
            })?;
        if source_seal.authorization != expected
            || source_seal.artifact_release_id.trim().is_empty()
            || source_seal.source_journal_sha256.len() != 64
            || !source_seal
                .source_journal_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(AppOperationError::InvalidState(
                "Generation source seal identity or evidence mismatch".into(),
            ));
        }
        let runner = BoundManagementRunner {
            service: self,
            context,
            target,
        };
        let command = "curl --silent --show-error --fail-with-body --connect-timeout 3 --max-time 5 --noproxy '*' -H \"x-deploy-token: $APP_CLI_DEPLOY_TOKEN\" http://127.0.0.1:3010/v1/runtime/configuration/prepared";
        loop {
            let reply = tokio::time::timeout_at(deadline, runner.run(command)).await;
            if let Ok(Ok(reply)) = reply {
                if reply.exit_code == 22 {
                    return Err(AppOperationError::InvalidState("Generation handoff rejected by management owner before credential mutation".into()));
                }
                if reply.exit_code != 0 {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep_until(std::cmp::min(
                        deadline,
                        tokio::time::Instant::now() + std::time::Duration::from_millis(250),
                    ))
                    .await;
                    continue;
                }
                let envelope: serde_json::Value =
                    serde_json::from_str(&reply.stdout).map_err(|_| {
                        AppOperationError::InvalidState(
                            "Malformed handoff preparation response".into(),
                        )
                    })?;
                if envelope["code"] == "HANDOFF_PENDING" && envelope["success"] == false {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep_until(std::cmp::min(
                        deadline,
                        tokio::time::Instant::now() + std::time::Duration::from_millis(250),
                    ))
                    .await;
                    continue;
                }
                if envelope["success"] == true && envelope["data"]["activated"] == true {
                    let acknowledged: shared_types::RuntimeGenerationHandoff =
                        serde_json::from_value(envelope["data"]["authorization"].clone()).map_err(
                            |_| {
                                AppOperationError::InvalidState(
                                    "Malformed consumed handoff identity".into(),
                                )
                            },
                        )?;
                    if acknowledged != expected {
                        return Err(AppOperationError::InvalidState(
                            "Consumed handoff identity mismatch".into(),
                        ));
                    }
                    return Ok(true);
                }
                let prepared: shared_types::RuntimeGenerationPrepared =
                    serde_json::from_value(envelope["data"].clone()).map_err(|_| {
                        AppOperationError::InvalidState(
                            "Invalid handoff preparation receipt".into(),
                        )
                    })?;
                let digest = |value: &str| {
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                };
                if envelope["success"] != true
                    || prepared.authorization != expected
                    || prepared.artifact_release_id != source_seal.artifact_release_id
                    || prepared.previous_journal_sha256 != source_seal.source_journal_sha256
                    || prepared.desired_revision != source_seal.desired_revision
                    || prepared.artifact_release_id.trim().is_empty()
                    || prepared.execution_workspace.trim().is_empty()
                    || !digest(&prepared.release_manifest_sha256)
                    || !digest(&prepared.previous_journal_sha256)
                {
                    return Err(AppOperationError::InvalidState(
                        "Generation handoff preparation evidence mismatch".into(),
                    ));
                }
                return Ok(false);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppOperationError::InvalidState("Generation handoff did not confirm before credential mutation; original operation remains protected".into()));
            }
            tokio::time::sleep_until(std::cmp::min(
                deadline,
                tokio::time::Instant::now() + std::time::Duration::from_millis(250),
            ))
            .await;
        }
        Err(AppOperationError::InvalidState(
            "Generation handoff did not confirm before credential mutation".into(),
        ))
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

fn source_seal_reply(
    text: &str,
    authorization: &shared_types::RuntimeGenerationHandoff,
) -> AppResult<Option<shared_types::RuntimeGenerationSourceSeal>> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| {
        AppOperationError::InvalidState(
            "Malformed source seal response; original operation remains protected".into(),
        )
    })?;
    if value.get("success").and_then(serde_json::Value::as_bool) == Some(false)
        && value.get("code").and_then(serde_json::Value::as_str) == Some("HANDOFF_SOURCE_BUSY")
    {
        return Ok(None);
    }
    if value.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(AppOperationError::InvalidState(
            "Source generation cannot be sealed; original operation remains protected".into(),
        ));
    }
    let seal: shared_types::RuntimeGenerationSourceSeal =
        serde_json::from_value(value.get("data").cloned().unwrap_or_default()).map_err(|_| {
            AppOperationError::InvalidState(
                "Invalid source seal receipt; original operation remains protected".into(),
            )
        })?;
    if seal.authorization != *authorization
        || seal.artifact_release_id.trim().is_empty()
        || seal.source_journal_sha256.len() != 64
        || !seal
            .source_journal_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AppOperationError::InvalidState(
            "Source seal identity or evidence mismatch; original operation remains protected"
                .into(),
        ));
    }
    Ok(Some(seal))
}

#[cfg(test)]
mod handoff_tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn source_seal_precedes_replacement_and_unknown_ack_preserves_fence() {
        use std::sync::atomic::Ordering;
        for fault in [
            "busy-then-sealed",
            "busy-deadline",
            "disconnect",
            "wrong-receipt",
            "preparation-failed",
            "recovery-old",
            "recovery-new",
            "recovery-third",
            "offline",
            "offline-run-unknown",
            "offline-cleanup-unknown",
        ] {
            let root = tempfile::tempdir().unwrap();
            let runtime = Arc::new(crate::test_support::MockRuntime::default());
            let service = crate::test_support::test_service(root.path(), runtime.clone()).await;
            let app_id = "sourcesealtest";
            let identity = service
                .metadata
                .store
                .ensure_identity(app_id)
                .await
                .unwrap();
            // Deliberately different workload and Pod identities, as in K8s.
            *runtime.mutation_uid_override.lock().unwrap() = Some("workloaduid".into());
            let current = container_runtime_api::DeploymentStatus {
                app_id: app_id.into(),
                replicas: if fault.starts_with("offline") { 0 } else { 1 },
                ready_replicas: if fault.starts_with("offline") { 0 } else { 1 },
                phase: if fault.starts_with("offline") {
                    "Stopped"
                } else {
                    "Running"
                }
                .into(),
                ..Default::default()
            };
            runtime.deployments.insert(app_id.into(), current.clone());
            runtime.specs.insert(
                app_id.into(),
                container_runtime_api::ContainerSpecSnapshot {
                    env: Some(
                        [(
                            shared_types::APP_DEPLOY_GENERATION_ID.into(),
                            "oldgeneration".into(),
                        )]
                        .into(),
                    ),
                    ..Default::default()
                },
            );
            let guard = service
                .try_acquire_process_release_lock(app_id)
                .await
                .unwrap();
            // Docker uses the real file guard above. This runtime fixture also
            // requires an explicit management lease (its Pod exec contract), so
            // acquire it normally instead of faking its lease_held assertion.
            let _management_lease = service
                .runtime
                .acquire_app_operation(app_id)
                .await
                .unwrap()
                .unwrap();
            let mut operation = crate::service::OwnedOperation::admit(
                service.metadata.store.clone(),
                shared_types::UserAppAdmission {
                    app_id: app_id.into(),
                    lifecycle_id: Some(identity.lifecycle_id.clone()),
                    operation_id: "sealoperation".into(),
                    request_id: None,
                    request_fingerprint: "a".repeat(64),
                    kind: shared_types::UserAppOperationKind::StartDeployment,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
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
                    chrono::Utc::now().timestamp_millis()
                        + if fault == "busy-deadline" { 150 } else { 3_000 },
                )
                .await
                .unwrap();
            let mut authorization = shared_types::RuntimeGenerationHandoff {
                protocol_version: 1,
                app_id: app_id.into(),
                lifecycle_id: identity.lifecycle_id,
                previous_generation: "oldgeneration".into(),
                previous_resource_uid: "workloaduid".into(),
                previous_resource_name: app_id.into(),
                activation: shared_types::RuntimeConfigurationActivation {
                    operation_id: context.operation_id.clone(),
                    deployment_generation: context.operation_id.clone(),
                    config_version: 1,
                },
            };
            if fault == "wrong-receipt" {
                authorization.activation.operation_id = "otheroperation".into();
            }
            let reply = |stdout: String, exit_code| container_runtime_api::ExecResult {
                stdout,
                exit_code,
                stderr: String::new(),
            };
            if fault.starts_with("busy") {
                runtime
                    .configuration_replies
                    .lock()
                    .unwrap()
                    .push_back(reply(
                        serde_json::json!({"success":false,"code":"HANDOFF_SOURCE_BUSY"})
                            .to_string(),
                        0,
                    ));
            }
            if fault != "busy-deadline" {
                runtime.configuration_replies.lock().unwrap().push_back(reply(serde_json::json!({"success":true,"data":shared_types::RuntimeGenerationSourceSeal {
                    authorization, artifact_release_id: "latesthotrelease".into(), source_journal_sha256: "b".repeat(64), desired_revision: 4,
                }}).to_string(), if fault == "disconnect" { 7 } else { 0 }));
            }
            runtime.patch_preparation_fails.store(
                matches!(fault, "preparation-failed" | "recovery-old"),
                Ordering::SeqCst,
            );
            runtime
                .offline_source_run_fails
                .store(fault == "offline-run-unknown", Ordering::SeqCst);
            runtime
                .offline_source_cleanup_fails
                .store(fault == "offline-cleanup-unknown", Ordering::SeqCst);
            let params = container_runtime_api::ContainerCreateParams::builder()
                .project_id(app_id)
                .service_type(shared_types::ServiceType::Userapp)
                .env(
                    [
                        (
                            shared_types::APP_RUNTIME_CONFIGURATION_VERSION.into(),
                            "1".into(),
                        ),
                        (
                            shared_types::APP_DEPLOY_GENERATION_ID.into(),
                            context.operation_id.clone(),
                        ),
                        (
                            shared_types::APP_DEPLOY_OPERATION_ID.into(),
                            context.operation_id.clone(),
                        ),
                    ]
                    .into(),
                )
                .build();
            let result = service
                .execute_update(
                    app_id,
                    params.clone(),
                    current.clone(),
                    &mut operation,
                    &guard,
                )
                .await;
            let commands = runtime.configuration_commands.lock().unwrap().clone();
            if fault.starts_with("offline") {
                assert!(
                    commands.is_empty(),
                    "stopped source must not use active-owner exec"
                );
                assert_eq!(runtime.management_start_calls.load(Ordering::SeqCst), 0);
                assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
                let before = service
                    .metadata
                    .store
                    .get_operation(app_id, &context.operation_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    before.checkpoint["offline_source_helper"]["helper_uid"],
                    "originalhelperuid"
                );
                if fault == "offline" {
                    result.unwrap();
                } else {
                    assert!(result.is_err());
                    assert!(guard.has_unfinished_mutation());
                    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
                    runtime
                        .offline_source_run_fails
                        .store(false, Ordering::SeqCst);
                    runtime
                        .offline_source_cleanup_fails
                        .store(false, Ordering::SeqCst);
                    service
                        .execute_update(app_id, params, current, &mut operation, &guard)
                        .await
                        .unwrap();
                }
                assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1);
                let calls = runtime.offline_source_calls.lock().unwrap().clone();
                let expected = match fault {
                    "offline-run-unknown" => vec!["prepare", "run", "run", "cleanup"],
                    "offline-cleanup-unknown" => vec!["prepare", "run", "cleanup", "cleanup"],
                    _ => vec!["prepare", "run", "cleanup"],
                };
                assert_eq!(calls, expected, "{fault}");
                let after = service
                    .metadata
                    .store
                    .get_operation(app_id, &context.operation_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    after.checkpoint["generation_handoff"],
                    before.checkpoint["generation_handoff"]
                );
                assert_eq!(
                    after.checkpoint["offline_source_helper"],
                    before.checkpoint["offline_source_helper"]
                );
                continue;
            }
            assert!(!commands.is_empty(), "must reach source owner: {fault}");
            assert!(commands.iter().all(|command| {
                command
                    .last()
                    .unwrap()
                    .contains("/configuration/source-seal")
            }));
            let record = service
                .metadata
                .store
                .get_operation(app_id, &context.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                record.checkpoint["source_management_target"]["physical_uid"],
                format!("test-{}", context.lifecycle_id)
            );
            if fault == "busy-then-sealed" || fault.starts_with("recovery") {
                if fault == "recovery-old" {
                    assert!(result.is_err());
                } else {
                    result.unwrap();
                }
                if fault == "busy-then-sealed" {
                    assert_eq!(commands.len(), 2);
                    assert_eq!(
                        commands[0], commands[1],
                        "retry must preserve original request"
                    );
                }
                assert_eq!(
                    runtime.create_calls.load(Ordering::SeqCst),
                    usize::from(fault != "recovery-old")
                );
                assert_eq!(
                    record.checkpoint["source_seal"]["artifact_release_id"],
                    "latesthotrelease"
                );
                if fault.starts_with("recovery") {
                    runtime
                        .patch_preparation_fails
                        .store(false, Ordering::SeqCst);
                    if fault != "recovery-old" {
                        let installed =
                            runtime.create_params_history.get(app_id).unwrap()[0].clone();
                        let mut installed_env = installed.env.clone().unwrap();
                        if fault == "recovery-third" {
                            installed_env.insert(
                                shared_types::APP_DEPLOY_GENERATION_ID.into(),
                                "thirdgeneration".into(),
                            );
                        }
                        runtime.specs.insert(
                            app_id.into(),
                            container_runtime_api::ContainerSpecSnapshot {
                                env: Some(installed_env),
                                ..Default::default()
                            },
                        );
                    } else {
                        runtime.configuration_replies.lock().unwrap().push_back(reply(
                            serde_json::json!({"success":true,"data":record.checkpoint["source_seal"]}).to_string(), 0));
                    }
                    let resumed = service
                        .execute_update(app_id, params, current, &mut operation, &guard)
                        .await;
                    assert_eq!(
                        resumed.is_ok(),
                        fault != "recovery-third",
                        "{fault}: {resumed:?}"
                    );
                    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1, "{fault}");
                    let resumed_commands = runtime.configuration_commands.lock().unwrap().clone();
                    assert_eq!(
                        resumed_commands.len(),
                        if fault == "recovery-old" { 2 } else { 1 }
                    );
                    if fault == "recovery-old" {
                        assert_eq!(resumed_commands[0], resumed_commands[1]);
                    }
                    let resumed_record = service
                        .metadata
                        .store
                        .get_operation(app_id, &context.operation_id)
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        resumed_record.checkpoint["generation_handoff"],
                        record.checkpoint["generation_handoff"]
                    );
                    assert_eq!(
                        resumed_record.checkpoint["source_management_target"],
                        record.checkpoint["source_management_target"]
                    );
                    assert!(guard.has_unfinished_mutation());
                }
            } else {
                assert!(result.is_err(), "{fault}");
                assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0, "{fault}");
                assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0, "{fault}");
                assert_eq!(
                    guard.has_unfinished_mutation(),
                    fault != "busy-deadline",
                    "{fault}"
                );
            }
        }
    }

    #[tokio::test]
    async fn handoff_identity_failure_or_disconnect_never_reaches_pg() {
        for fault in [
            "wrong-operation",
            "wrong-physical-source",
            "wrong-journal-digest",
            "wrong-artifact",
            "wrong-desired-revision",
            "rejected",
            "disconnect",
            "consumed",
        ] {
            let root = tempfile::tempdir().unwrap();
            let runtime = Arc::new(crate::test_support::MockRuntime::default());
            let mut service = crate::test_support::test_service(root.path(), runtime.clone()).await;
            service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
            let app_id = "handofftest";
            let identity = service
                .metadata
                .store
                .ensure_identity(app_id)
                .await
                .unwrap();
            service
                .runtime_configuration
                .save_runtime_configuration(
                    app_id,
                    shared_types::UserAppOperationScope::Prod,
                    &shared_types::SaveRuntimeConfigurationRequest {
                        lifecycle_id: identity.lifecycle_id.clone(),
                        request_id: "save".into(),
                        expected_revision: 0,
                        pg: StartPgCredential {
                            username: "business".into(),
                            password: "test-password".into(),
                        },
                    },
                )
                .await
                .unwrap();
            let guard = service
                .try_acquire_process_release_lock(app_id)
                .await
                .unwrap();
            let mut operation = crate::service::OwnedOperation::admit(
                service.metadata.store.clone(),
                shared_types::UserAppAdmission {
                    app_id: app_id.into(),
                    lifecycle_id: Some(identity.lifecycle_id.clone()),
                    operation_id: "handoff-operation".into(),
                    request_id: None,
                    request_fingerprint: "a".repeat(64),
                    kind: shared_types::UserAppOperationKind::StartDeployment,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                },
            )
            .await
            .unwrap();
            operation.bind_lease(&guard).await.unwrap();
            let context = operation.execution_context();
            let authorization = shared_types::RuntimeGenerationHandoff {
                protocol_version: 1,
                app_id: app_id.into(),
                lifecycle_id: identity.lifecycle_id.clone(),
                previous_generation: "old-generation".into(),
                previous_resource_uid: "old-physical-uid".into(),
                previous_resource_name: "old-workload".into(),
                activation: shared_types::RuntimeConfigurationActivation {
                    operation_id: context.operation_id.clone(),
                    deployment_generation: context.operation_id.clone(),
                    config_version: 1,
                },
            };
            operation
                .checkpoint(
                    "runtime_updated",
                    serde_json::json!({"generation_handoff":authorization, "source_seal": shared_types::RuntimeGenerationSourceSeal {
                        authorization: authorization.clone(), artifact_release_id: "hot-b".into(),
                        source_journal_sha256: "b".repeat(64), desired_revision: 2,
                    }}),
                )
                .await
                .unwrap();
            service
                .metadata
                .store
                .bind_operation_deadline(
                    app_id,
                    &context.operation_id,
                    &context.lifecycle_id,
                    chrono::Utc::now().timestamp_millis() + 1_000,
                )
                .await
                .unwrap();
            if fault == "consumed" {
                let target = service
                    .runtime
                    .capture_app_configuration_target(&context, &context.operation_id)
                    .await
                    .unwrap();
                service
                    .runtime_configuration
                    .bind_runtime_configuration_target(&context, 1, &target)
                    .await
                    .unwrap();
                service
                    .runtime_configuration
                    .record_runtime_configuration_result(
                        &context,
                        1,
                        &target,
                        CredentialApplicationState::Applying,
                        BusinessStartupState::NotStarted,
                    )
                    .await
                    .unwrap();
                service
                    .runtime_configuration
                    .record_runtime_configuration_result(
                        &context,
                        1,
                        &target,
                        CredentialApplicationState::Applied,
                        BusinessStartupState::Starting,
                    )
                    .await
                    .unwrap();
                service
                    .runtime_configuration
                    .record_runtime_configuration_result(
                        &context,
                        1,
                        &target,
                        CredentialApplicationState::Applied,
                        BusinessStartupState::Unknown,
                    )
                    .await
                    .unwrap();
                runtime.configuration_replies.lock().unwrap().extend([
                    serde_json::json!({"success":true,"data":{"activated":true,"authorization":authorization}}).to_string(),
                    serde_json::json!({"status":"ready","phase":"running"}).to_string(),
                ].into_iter().map(|stdout| container_runtime_api::ExecResult { exit_code: 0, stdout, stderr: String::new() }));
                service
                    .complete_cold_runtime_configuration(&context)
                    .await
                    .unwrap();
                let commands = runtime.configuration_commands.lock().unwrap().clone();
                assert_eq!(commands.len(), 2);
                assert!(
                    commands[0]
                        .last()
                        .unwrap()
                        .contains("/configuration/prepared")
                );
                assert!(commands[1].last().unwrap().ends_with("/ready"));
                drop(commands);
                let capture = service
                    .runtime_configuration
                    .operation_runtime_configuration(&context)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(capture.credentials, CredentialApplicationState::Applied);
                assert_eq!(capture.business, BusinessStartupState::Ready);
                continue;
            }
            let mut prepared = shared_types::RuntimeGenerationPrepared {
                authorization,
                artifact_release_id: "hot-b".into(),
                release_manifest_sha256: "a".repeat(64),
                previous_journal_sha256: "b".repeat(64),
                execution_workspace: "/app/code/.run".into(),
                desired_revision: 2,
            };
            match fault {
                "wrong-operation" => {
                    prepared.authorization.activation.operation_id = "other".into()
                }
                "wrong-physical-source" => {
                    prepared.authorization.previous_resource_uid = "replacement-impostor".into()
                }
                "wrong-journal-digest" => prepared.previous_journal_sha256 = "c".repeat(64),
                "wrong-artifact" => prepared.artifact_release_id = "staleartifact".into(),
                "wrong-desired-revision" => prepared.desired_revision += 1,
                _ => {}
            }
            runtime.configuration_replies.lock().unwrap().push_back(
                container_runtime_api::ExecResult {
                    exit_code: match fault {
                        "rejected" => 22,
                        "disconnect" => 7,
                        _ => 0,
                    },
                    stdout: serde_json::json!({"success":true,"data":prepared}).to_string(),
                    stderr: String::new(),
                },
            );
            assert!(
                service
                    .complete_cold_runtime_configuration(&context)
                    .await
                    .is_err(),
                "{fault}"
            );
            let commands = runtime.configuration_commands.lock().unwrap().clone();
            assert!(!commands.is_empty());
            assert!(
                commands
                    .iter()
                    .all(|command| command.last().unwrap().contains("/configuration/prepared")),
                "{fault}"
            );
            drop(commands);
            let capture = service
                .runtime_configuration
                .operation_runtime_configuration(&context)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                capture.credentials,
                CredentialApplicationState::Captured,
                "{fault}"
            );
            assert_eq!(
                capture.business,
                BusinessStartupState::NotStarted,
                "{fault}"
            );
        }
    }
}
